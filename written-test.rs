crate fmt;

struct Terminal {
    
} 


fn get_terminal_size() -> size {
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    size { width, height }
}
fn repaint_terminal() {
    // Clear the screen and redraw
    print!("\x1b[2J\x1b[H");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
}

fn toggle_cursor_visibility(visible: bool) {
    if visible {
        print!("\x1b[?25h"); // Show cursor
    } else {
        print!("\x1b[?25l"); // Hide cursor
    }
    std::io::Write::flush(&mut std::io::stdout()).ok();
}

fn color_text(text: &str, color: u8) -> String {
    format!("\x1b[{}m{}\x1b[0m", color, text)
}

fn get_terminal_type() -> String {
    std::env::var("TERM").unwrap_or_else(|_| "unknown".to_string())
}
