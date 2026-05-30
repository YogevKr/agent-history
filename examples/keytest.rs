use crossterm::{
    event::{self, Event, KeyEventKind},
    terminal,
};

fn main() {
    terminal::enable_raw_mode().unwrap();
    println!("Press keys to see events. Press 'q' to quit.\r");
    println!("Try: Ctrl-y, Ctrl-r, arrows, Enter\r");
    println!("---\r");
    loop {
        if let Ok(Event::Key(ke)) = event::read() {
            println!(
                "code={:?}  mod={:?}  kind={:?}\r",
                ke.code, ke.modifiers, ke.kind
            );
            if ke.kind == KeyEventKind::Press {
                if let crossterm::event::KeyCode::Char('q') = ke.code {
                    if ke.modifiers.is_empty() {
                        break;
                    }
                }
            }
        }
    }
    terminal::disable_raw_mode().unwrap();
}
