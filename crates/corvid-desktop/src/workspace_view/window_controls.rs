use gpui::{
    Deferred, Div, PathBuilder, Stateful, Window, WindowControlArea, canvas, deferred, div, point,
    prelude::*, px, rgb,
};

use super::{BORDER, PANEL, TEXT};

pub(super) fn window_drag_region(region: Div) -> Div {
    region.window_control_area(WindowControlArea::Drag)
}

pub(super) fn render(window: &Window) -> Deferred {
    let maximize = if window.is_maximized() {
        CaptionButton::Restore
    } else {
        CaptionButton::Maximize
    };
    // Keep the caption buttons hittable above the workspace's menu backdrops.
    deferred(
        div()
            .flex()
            .flex_none()
            .h_full()
            .bg(rgb(PANEL))
            .occlude()
            .child(CaptionButton::Minimize.render())
            .child(maximize.render())
            .child(CaptionButton::Close.render()),
    )
}

#[derive(Clone, Copy)]
enum CaptionButton {
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl CaptionButton {
    fn render(self) -> Stateful<Div> {
        let (id, area, hover, pressed) = match self {
            Self::Minimize => ("minimize-window", WindowControlArea::Min, BORDER, 0x414754),
            Self::Maximize | Self::Restore => {
                ("maximize-window", WindowControlArea::Max, BORDER, 0x414754)
            }
            Self::Close => ("close-window", WindowControlArea::Close, 0xb83243, 0x962938),
        };
        // GPUI dispatches these non-client controls through Windows, including WM_CLOSE.
        div()
            .id(id)
            .window_control_area(area)
            .flex()
            .flex_none()
            .w(px(46.))
            .h_full()
            .items_center()
            .justify_center()
            .hover(|style| style.bg(rgb(hover)))
            .active(|style| style.bg(rgb(pressed)))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        let at = |x, y| bounds.origin + point(px(x), px(y));
                        let mut path = PathBuilder::stroke(px(1.));
                        match self {
                            Self::Minimize => {
                                path.move_to(at(0.5, 5.5));
                                path.line_to(at(9.5, 5.5));
                            }
                            Self::Maximize => {
                                path.move_to(at(0.5, 0.5));
                                path.line_to(at(9.5, 0.5));
                                path.line_to(at(9.5, 9.5));
                                path.line_to(at(0.5, 9.5));
                                path.close();
                            }
                            Self::Restore => {
                                path.move_to(at(2.5, 2.5));
                                path.line_to(at(2.5, 0.5));
                                path.line_to(at(9.5, 0.5));
                                path.line_to(at(9.5, 7.5));
                                path.line_to(at(7.5, 7.5));
                                path.move_to(at(0.5, 2.5));
                                path.line_to(at(7.5, 2.5));
                                path.line_to(at(7.5, 9.5));
                                path.line_to(at(0.5, 9.5));
                                path.close();
                            }
                            Self::Close => {
                                path.move_to(at(0.5, 0.5));
                                path.line_to(at(9.5, 9.5));
                                path.move_to(at(9.5, 0.5));
                                path.line_to(at(0.5, 9.5));
                            }
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, rgb(TEXT));
                        }
                    },
                )
                .size(px(10.)),
            )
    }
}
