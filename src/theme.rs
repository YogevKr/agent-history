//! Terminal color theme with auto dark/light detection.

use std::sync::OnceLock;

pub struct Theme {
    pub accent: (u8, u8, u8),
    pub text_primary: (u8, u8, u8),
    pub text_secondary: (u8, u8, u8),
    pub text_muted: (u8, u8, u8),
    pub heading: (u8, u8, u8),
    pub code_inline: (u8, u8, u8),
    pub code_block_fg: (u8, u8, u8),
    pub tool_color: (u8, u8, u8),
    pub border: (u8, u8, u8),
    pub user_color: (u8, u8, u8),
    pub assistant_color: (u8, u8, u8),
    pub diff_add: (u8, u8, u8),
    pub diff_remove: (u8, u8, u8),
    pub list_bullet: (u8, u8, u8),
    pub syntect_theme: &'static str,
}

static THEME: OnceLock<Theme> = OnceLock::new();

pub fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        if is_light_terminal() {
            light_theme()
        } else {
            dark_theme()
        }
    })
}

fn is_light_terminal() -> bool {
    match terminal_light::luma() {
        Ok(luma) => luma > 0.6,
        Err(_) => false, // default dark
    }
}

fn dark_theme() -> Theme {
    Theme {
        accent: (78, 201, 176),        // teal
        text_primary: (212, 212, 212),
        text_secondary: (160, 160, 160),
        text_muted: (100, 100, 100),
        heading: (180, 190, 220),       // pale blue
        code_inline: (147, 161, 199),   // purple-blue
        code_block_fg: (147, 161, 199),
        tool_color: (206, 172, 105),    // warm yellow
        border: (70, 70, 70),
        user_color: (80, 200, 120),     // green
        assistant_color: (78, 201, 176), // teal
        diff_add: (80, 200, 120),
        diff_remove: (220, 80, 80),
        list_bullet: (100, 100, 100),
        syntect_theme: "base16-ocean.dark",
    }
}

fn light_theme() -> Theme {
    Theme {
        accent: (0, 128, 128),
        text_primary: (30, 30, 30),
        text_secondary: (80, 80, 80),
        text_muted: (140, 140, 140),
        heading: (50, 60, 120),
        code_inline: (80, 70, 140),
        code_block_fg: (80, 70, 140),
        tool_color: (160, 120, 20),
        border: (180, 180, 180),
        user_color: (0, 130, 50),
        assistant_color: (0, 128, 128),
        diff_add: (0, 130, 50),
        diff_remove: (180, 30, 30),
        list_bullet: (140, 140, 140),
        syntect_theme: "InspiredGitHub",
    }
}
