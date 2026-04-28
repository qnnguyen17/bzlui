mod app;
mod bazel;
mod ui;

use std::io;
use std::path::PathBuf;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use tokio_stream::StreamExt;

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let workspace_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    println!("Loading workspace {workspace_dir:#?}...");
    let trie = bazel::load_workspace(&workspace_dir)?;

    let mut app = app::AppState::new(trie, workspace_dir);

    let mut main_view = ui::MainView::new(&mut app);

    let mut terminal = ratatui::init();
    let mut events = EventStream::new();

    loop {
        terminal.draw(|frame| frame.render_widget(&mut main_view, frame.area()))?;

        tokio::select! {
            maybe_event = events.next() => {
                let Some(Ok(Event::Key(key))) = maybe_event else { continue };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let handled = main_view.handle_key(key);
                if !handled && key.code == KeyCode::Char('q') {
                    break;
                }
            }
            Some(update) = main_view.next_modal_update() => {
                main_view.apply_modal_update(update);
            }
        }
    }

    ratatui::restore();
    Ok(())
}
