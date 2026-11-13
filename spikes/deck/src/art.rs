pub fn outline(glyph: &str) -> Option<&'static str> {
    Some(match glyph {
        "brand" => {
            "M5 19.5 6.6 9.3 9.4 5.5h4.7l3.2 2.6 4 2.5-6 1-.8 3.6-4 4.3.2-3.2Z M12.4 8.9l1.2-.3"
        }
        "grid" => "M4 4h6v6H4z M14 4h6v6h-6z M4 14h6v6H4z M14 14h6v6h-6z",
        "focus" => {
            "M8 3H5a2 2 0 0 0-2 2v3 M16 3h3a2 2 0 0 1 2 2v3 M3 16v3a2 2 0 0 0 2 2h3 M21 16v3a2 2 0 0 1-2 2h-3 M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8Z"
        }
        "capture" => {
            "M4 7h4l2-3h4l2 3h4a1 1 0 0 1 1 1v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a1 1 0 0 1 1-1Z M12 10a3.5 3.5 0 1 0 0 7 3.5 3.5 0 0 0 0-7Z"
        }
        "notes" => {
            "M6 3h12a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2Z M8 8h8 M8 12h8 M8 16h5"
        }
        "tasks" => {
            "m3 6 1.5 1.5L7 5 M10 6h11 M3 12l1.5 1.5L7 11 M10 12h11 M3 18l1.5 1.5L7 17 M10 18h11"
        }
        "timer" => "M9 2h6 M12 2v3 M18 7l2-2 M12 9v4l3 2 M12 5a8 8 0 1 0 0 16 8 8 0 0 0 0-16Z",
        "mic" => "M9 5a3 3 0 0 1 6 0v7a3 3 0 0 1-6 0V5Z M5 11v1a7 7 0 0 0 14 0v-1 M12 19v3 M8 22h8",
        "camera" => {
            "M4 6h11a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2Z M17 10l5-3v10l-5-3"
        }
        "volume-down" => "M4 9h4l5-4v14l-5-4H4Z M17 12h4",
        "volume-up" => "M4 9h4l5-4v14l-5-4H4Z M17 12h5 M19.5 9.5v5",
        "mute" => "M4 9h4l5-4v14l-5-4H4Z M17 10l5 5 M22 10l-5 5",
        "previous" => "M5 5v14 M19 5 8 12l11 7V5Z",
        "play" => "M6 5v14l11-7L6 5Z M21 6v12",
        "next" => "M19 5v14 M5 5l11 7-11 7V5Z",
        "coffee" => {
            "M4 9h13v5a5 5 0 0 1-5 5H9a5 5 0 0 1-5-5V9Z M17 10h2a2 2 0 0 1 0 4h-2 M3 22h16 M8 3v3 M12 2v4"
        }
        "lock" => "M5 10h14v11H5z M8 10V7a4 4 0 1 1 8 0v3 M12 14v3",
        "moon" => "M20 15a8.7 8.7 0 0 1-11-11 9 9 0 1 0 11 11Z",
        "dim" => {
            "M12 3v2 M12 19v2 M3 12h2 M19 12h2 M5.5 5.5 7 7 M17 17l1.5 1.5 M5.5 18.5 7 17 M17 7l1.5-1.5 M9 12h6"
        }
        "sun" => {
            "M12 3v2 M12 19v2 M3 12h2 M19 12h2 M5.5 5.5 7 7 M17 17l1.5 1.5 M5.5 18.5 7 17 M17 7l1.5-1.5 M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8Z"
        }
        "settings" => "M4 6h16 M4 12h16 M4 18h16 M8 4v4 M16 10v4 M10 16v4",
        "bell" => "M5 17h14l-2-3V9a5 5 0 0 0-10 0v5l-2 3Z M10 20a2 2 0 0 0 4 0",
        "plane" => "M12 3v17 M3 14l9-6 9 6 M8 20l4-2 4 2",
        "bluetooth" => "m6 6 12 12-6 4V2l6 4L6 18",
        "wifi" => "M3 9a15 15 0 0 1 18 0 M6 13a10 10 0 0 1 12 0 M9 17a5 5 0 0 1 6 0 M12 20h.01",
        "battery" => "M3 7h16v10H3z M21 10v4 M10 9l-2 3h4l-2 3",
        "copy" => "M8 8h12v13H8z M16 4H4v13",
        "paste" => "M9 3h6v4H9z M8 5H5v16h14V5h-3 M8 11h8 M8 15h8",
        "undo" => "M8 5 3 10l5 5 M3 10h11a6 6 0 0 1 0 12",
        "redo" => "m16 5 5 5-5 5 M21 10H10a6 6 0 0 0 0 12",
        "activity" => "M3 12h4l3-7 4 14 3-7h4",
        "keyboard" => "M3 6h18v12H3z M6 10h1 M10 10h1 M14 10h1 M18 10h.01 M7 14h10",
        "page" => "M4 5h11v4 M4 5v14h11v-4 M10 12h11 M18 9l3 3-3 3",
        _ => return None,
    })
}

pub fn outline_svg(glyph: &str) -> Option<String> {
    let outline = outline(glyph)?;
    Some(format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="{outline}" fill="none" stroke="white" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>"#
    ))
}
