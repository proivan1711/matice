use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui_kit::component::Disableable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::*;
use matice::DownloadProgress;

struct Main {
    url: Entity<InputState>,
    folder: Option<PathBuf>,
    running: bool,
    progress: Option<Arc<DownloadProgress>>,
    rt: tokio::runtime::Runtime,
}

impl Main {
    fn pick_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await {
                this.update(cx, |this, cx| {
                    this.folder = paths.pop();
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(folder) = self.folder.clone() else {
            return;
        };
        let url = self.url.read(cx).value().to_string();
        if url.is_empty() {
            return;
        }

        let progress = Arc::new(DownloadProgress::default());
        self.progress = Some(progress.clone());
        self.running = true;
        cx.notify();

        let task = self
            .rt
            .spawn(async move { matice::download(&url, folder, progress).await });
        cx.spawn(async move |this, cx| {
            // Redraw every 100ms so the progress bar follows the download.
            while !task.is_finished() {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                this.update(cx, |_, cx| cx.notify()).ok();
            }
            this.update(cx, |this, cx| {
                this.running = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for Main {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let folder = match &self.folder {
            Some(path) => path.display().to_string(),
            None => "No folder selected".into(),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_2()
            .child(Input::new(&self.url))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("folder")
                            .label("Output folder")
                            .on_click(cx.listener(|this, _, _, cx| this.pick_folder(cx))),
                    )
                    .child(folder),
            )
            .child(
                Button::new("start")
                    .primary()
                    .label(if self.running {
                        "Downloading..."
                    } else {
                        "Start"
                    })
                    .disabled(self.running || self.folder.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
            )
            .children(self.progress.as_ref().map(|p| {
                let done = p.done.load(Ordering::Relaxed);
                let total = p.total.load(Ordering::Relaxed);
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Progress::new("progress")
                            .flex_1()
                            // Animate while the page list is still being fetched.
                            .loading(self.running && total == 0)
                            .value(if total == 0 {
                                0.0
                            } else {
                                done as f32 * 100.0 / total as f32
                            }),
                    )
                    .child(format!("{done}/{total}"))
            }))
    }
}

fn main() {
    application().with_assets(assets::Assets).run(|cx| {
        init(cx);

        open_window(WindowOptions::default(), cx, |window, cx| {
            let url = cx.new(|cx| InputState::new(window, cx).placeholder("URL"));
            cx.new(|_| Main {
                url,
                folder: None,
                running: false,
                progress: None,
                rt: tokio::runtime::Runtime::new().expect("Failed to start tokio runtime"),
            })
        })
        .expect("Failed to open window");
    });
}
