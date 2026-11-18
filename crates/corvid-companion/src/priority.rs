use std::time::{SystemTime, UNIX_EPOCH};

use corvid_protocol::wire::{PresentedRun, RunAvailability};

const MINUTE_MS: u64 = 60_000;
const REQUEST_PEAK_MS: u64 = 30 * MINUTE_MS;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Priority {
    score: f64,
    recent_request: Option<u64>,
}

impl Priority {
    pub fn for_run(run: &PresentedRun, online: bool, now: u64) -> Self {
        if !online || run.availability != RunAvailability::Available as i32 {
            return Self::default();
        }
        if run.pending_requests > 0 {
            let Some(since) = run.pending_since_unix_ms else {
                // Older companions can report a pending request without its age.
                return Self {
                    score: 60.,
                    recent_request: None,
                };
            };
            let age = now.saturating_sub(since);
            let score = if age <= REQUEST_PEAK_MS {
                60. + age as f64 / MINUTE_MS as f64
            } else {
                decay(90., age - REQUEST_PEAK_MS, 90)
            };
            return Self {
                score,
                recent_request: (age <= REQUEST_PEAK_MS).then_some(since),
            };
        }
        let score = run
            .new_result_unix_ms
            .map_or(0., |since| decay(50., now.saturating_sub(since), 30));
        Self {
            score,
            recent_request: None,
        }
    }
}

fn decay(initial: f64, age: u64, half_life_minutes: u64) -> f64 {
    initial * 0.5_f64.powf(age as f64 / (half_life_minutes * MINUTE_MS) as f64)
}

/// Project the existing order into a grid followed by its queue. A strictly
/// higher score can replace an unpinned card. Displaced cards join the tail.
pub fn prioritize<T: Copy>(
    order: &mut Vec<T>,
    capacity: usize,
    now: u64,
    describe: impl Fn(&T, u64) -> (bool, Priority),
) {
    let visible = capacity.min(order.len());
    if visible < order.len() {
        promote(order, visible, now, &describe);
    }
    let waiting: Vec<_> = order[..visible]
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let (pinned, priority) = describe(item, now);
            (!pinned)
                .then_some(priority.recent_request)
                .flatten()
                .map(|time| (index, time, *item))
        })
        .collect();
    let mut sorted = waiting.clone();
    sorted.sort_by_key(|&(_, time, _)| time.min(now));
    for ((slot, _, _), (_, _, item)) in waiting.into_iter().zip(sorted) {
        order[slot] = item;
    }
}

fn promote<T>(
    order: &mut Vec<T>,
    visible: usize,
    now: u64,
    describe: &impl Fn(&T, u64) -> (bool, Priority),
) {
    let entries: Vec<_> = order.iter().map(|item| describe(item, now)).collect();
    let mut victims: Vec<_> = (0..visible).filter(|&i| !entries[i].0).collect();
    victims.sort_by(|&a, &b| {
        entries[a]
            .1
            .score
            .total_cmp(&entries[b].1.score)
            .then(b.cmp(&a))
    });
    let mut candidates: Vec<_> = (visible..order.len()).collect();
    candidates.sort_by(|&a, &b| entries[b].1.score.total_cmp(&entries[a].1.score));
    let swaps: Vec<_> = victims
        .into_iter()
        .zip(candidates)
        .take_while(|&(slot, queued)| entries[queued].1.score > entries[slot].1.score)
        .collect();
    if swaps.is_empty() {
        return;
    }
    let mut displaced = vec![None; order.len() - visible];
    for (slot, queued) in swaps {
        order.swap(slot, queued);
        displaced[queued - visible] = Some(slot);
    }
    let queued = order.split_off(visible);
    let mut tail = Vec::new();
    for (item, slot) in queued.into_iter().zip(displaced) {
        if let Some(slot) = slot {
            tail.push((slot, item));
        } else {
            order.push(item);
        }
    }
    tail.sort_by_key(|(slot, _)| *slot);
    order.extend(tail.into_iter().map(|(_, item)| item));
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 240 * MINUTE_MS;

    fn run(waiting: bool, minutes_ago: u64) -> PresentedRun {
        let since = NOW - minutes_ago * MINUTE_MS;
        PresentedRun {
            availability: RunAvailability::Available as i32,
            pending_requests: u32::from(waiting),
            pending_since_unix_ms: waiting.then_some(since),
            new_result_unix_ms: (!waiting).then_some(since),
            ..Default::default()
        }
    }

    #[test]
    fn waiting_increases_through_thirty_minutes_then_decays_continuously() {
        let score = |age| Priority::for_run(&run(true, age), true, NOW).score;
        assert_eq!(
            [score(5), score(25), score(30), score(120)],
            [65., 85., 90., 45.]
        );
        assert!(score(29) < score(30));
        assert!(score(31) < score(30));
        assert!(score(31) > score(29));
        assert_eq!(Priority::for_run(&run(false, 60), true, NOW).score, 12.5);
        assert_eq!(Priority::for_run(&run(true, 25), false, NOW).score, 0.);
    }

    #[test]
    fn promotes_new_attention_and_appends_the_displaced_card_after_existing_queue() {
        let mut order = vec!['a', 'b', 'c', 'd', 'e'];
        let priority = |card: &char, now| {
            let score = match card {
                'a' => Priority::for_run(&run(false, 60), true, now),
                'e' => Priority::for_run(&run(true, 0), true, now),
                _ => Priority::default(),
            };
            (*card == 'c', score)
        };
        prioritize(&mut order, 3, NOW, priority);
        assert_eq!(order, ['a', 'e', 'c', 'd', 'b']);
        prioritize(&mut order, 3, NOW, priority);
        assert_eq!(order, ['a', 'e', 'c', 'd', 'b']);
    }

    #[test]
    fn older_waiting_cards_use_earlier_visible_positions_around_pins() {
        let mut order = vec!['a', 'b', 'c', 'd', 'e'];
        prioritize(&mut order, 4, NOW, |card, now| {
            let age = match card {
                'a' => 5,
                'c' => 29,
                'd' => 25,
                'e' => 1,
                _ => return (true, Priority::default()),
            };
            (*card == 'c', Priority::for_run(&run(true, age), true, now))
        });
        assert_eq!(order, ['d', 'b', 'c', 'a', 'e']);
    }
}
