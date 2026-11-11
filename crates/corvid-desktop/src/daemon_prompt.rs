use anyhow::{Result, ensure};
use corvid_protocol::ProtocolFault;
use tokio::sync::{mpsc, oneshot};

pub struct Prompt {
    pub title: &'static str,
    pub message: String,
    pub action: Option<&'static str>,
    pub decision: oneshot::Sender<bool>,
}

pub struct Prompts(pub mpsc::UnboundedSender<Prompt>);

#[derive(Clone, Copy)]
pub enum Operation {
    Install,
    Upgrade,
}

impl Prompts {
    pub async fn confirm(&self, operation: Operation, host: &str, detail: &str) -> Result<()> {
        let (title, action) = match operation {
            Operation::Install => ("Install Corvid daemon?", "Install"),
            Operation::Upgrade => ("Upgrade Corvid daemon?", "Upgrade"),
        };
        let approved = self
            .ask(title, format!("Target: {host}\n\n{detail}"), Some(action))
            .await;
        ensure!(approved, "Corvid daemon setup canceled.");
        Ok(())
    }

    pub async fn manual_upgrade(&self, host: &str, detail: &str) {
        self.ask(
            "Corvid daemon update required",
            format!("Target: {host}\n\n{detail}"),
            None,
        )
        .await;
    }

    async fn ask(
        &self,
        title: &'static str,
        message: String,
        action: Option<&'static str>,
    ) -> bool {
        let (decision, response) = oneshot::channel();
        self.0
            .send(Prompt {
                title,
                message,
                action,
                decision,
            })
            .is_ok()
            && response.await == Ok(true)
    }
}

pub fn has_fault(error: &anyhow::Error, code: &str) -> bool {
    error
        .downcast_ref::<ProtocolFault>()
        .is_some_and(|fault| fault.code == code)
}

pub fn needs_upgrade(error: &anyhow::Error) -> bool {
    has_fault(error, "ProtocolMismatch") || has_fault(error, "UpgradeRequired")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn setup_requires_an_affirmative_dialog_response() {
        for decision in [None, Some(false), Some(true)] {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let prompts = Prompts(sender);
            let (result, ()) = tokio::join!(
                prompts.confirm(
                    Operation::Install,
                    "work-host",
                    "Install the bundled daemon."
                ),
                async {
                    let prompt = receiver.recv().await.unwrap();
                    assert_eq!(prompt.action, Some("Install"));
                    assert!(prompt.message.contains("work-host"));
                    if let Some(approved) = decision {
                        prompt.decision.send(approved).unwrap();
                    }
                },
            );
            assert_eq!(result.is_ok(), decision == Some(true));
        }
    }
}
