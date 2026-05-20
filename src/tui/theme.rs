//! TUI Theme configuration
//!
//! Centralized theme system for consistent styling across the UI.
//! Supports multiple color depths for terminal compatibility.

use ratatui::style::{Color, Style};

use crate::config::theme::{AgentWorkingStyle, ThemeOverrides};

/// All recognised preset names, in display order.
pub const PRESET_NAMES: &[&str] = &[
    "(auto)",
    "basic",
    "indexed",
    "truecolor",
    "monokai-dimmed",
    "zedokai",
    "rose-pine",
];

/// Terminal color capability
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMode {
    /// Basic 16 ANSI colors (maximum compatibility)
    Basic,
    /// 256 color palette
    #[default]
    Indexed,
    /// True color (24-bit RGB)
    TrueColor,
}

impl ColorMode {
    /// Detect the best color mode for the current terminal
    pub fn detect() -> Self {
        // Check COLORTERM first (most reliable for true color)
        if let Ok(colorterm) = std::env::var("COLORTERM")
            && (colorterm == "truecolor" || colorterm == "24bit")
        {
            return Self::TrueColor;
        }

        // Check TERM for 256 color support
        if let Ok(term) = std::env::var("TERM")
            && (term.contains("256color") || term.contains("kitty") || term.contains("alacritty"))
        {
            // These terminals typically support true color even without COLORTERM
            if term.contains("kitty") || term.contains("alacritty") {
                return Self::TrueColor;
            }
            return Self::Indexed;
        }

        Self::Basic
    }
}

/// Theme configuration for the TUI
#[derive(Clone)]
pub struct Theme {
    // Pane borders
    pub border_focused: Color,
    pub border_unfocused: Color,

    // Selection
    pub selection_bg: Color,
    pub selection_fg: Option<Color>,

    // Session status indicators
    pub status_creating: Color,
    pub status_running: Color,
    pub status_stopped: Color,
    pub status_pr: Color,
    pub status_pr_merged: Color,

    // PR badge text colours (per state). `status_pr` is reused for the
    // "open + awaiting review" colour.
    pub pr_open: Color,
    pub pr_draft: Color,
    pub pr_closed: Color,

    // PR label pill backgrounds (darker variants of the text colours above)
    // used when `invert_pr_label_color = false`.
    pub pr_pill_open_bg: Color,
    pub pr_pill_draft_bg: Color,
    pub pr_pill_closed_bg: Color,
    pub pr_pill_review_bg: Color,
    pub pr_pill_merged_bg: Color,
    /// Text colour used on top of PR pill backgrounds.
    pub pr_pill_text: Color,

    // Agent state and notification indicators
    pub agent_working: AgentWorkingStyle,
    pub agent_waiting: Color,
    pub unread_indicator: Color,

    // Text
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_accent: Color,
    pub project_colors: Vec<(Color, Color)>, // (project_header, session_title)

    // Diff colors
    pub diff_added: Color,
    pub diff_removed: Color,
    pub diff_hunk_header: Color,
    pub diff_file_header: Color,
    pub diff_context: Color,

    // Modal borders
    pub modal_info: Color,
    pub modal_warning: Color,
    pub modal_error: Color,

    // Quick-switch palette: background + text for *command* rows, so they
    // stand out visually from session rows in the unified palette view.
    pub palette_command_bg: Color,
    pub palette_command_fg: Color,

    // Status bar
    pub status_bar_bg: Color,
    pub status_bar_fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::for_color_mode(ColorMode::detect())
    }
}

impl Theme {
    /// Create a theme for the specified color mode
    pub fn for_color_mode(mode: ColorMode) -> Self {
        match mode {
            ColorMode::Basic => Self::basic(),
            ColorMode::Indexed => Self::indexed(),
            ColorMode::TrueColor => Self::truecolor(),
        }
    }

    /// Basic 16-color theme (maximum compatibility)
    pub fn basic() -> Self {
        Self {
            border_focused: Color::Cyan,
            border_unfocused: Color::DarkGray,

            selection_bg: Color::Blue,
            selection_fg: Some(Color::White),

            status_creating: Color::Yellow,
            status_running: Color::Green,
            status_stopped: Color::DarkGray,
            status_pr: Color::Green,
            status_pr_merged: Color::DarkGray,

            pr_open: Color::Blue,
            pr_draft: Color::DarkGray,
            pr_closed: Color::Red,

            // Basic 16-colour palette: the non-"Light" ANSI colours are
            // already dark enough to read white bold text on top.
            pr_pill_open_bg: Color::Blue,
            pr_pill_draft_bg: Color::DarkGray,
            pr_pill_closed_bg: Color::Red,
            pr_pill_review_bg: Color::Green,
            pr_pill_merged_bg: Color::Magenta,
            pr_pill_text: Color::White,

            agent_working: AgentWorkingStyle::Rainbow,
            agent_waiting: Color::Yellow,
            unread_indicator: Color::Blue,

            text_primary: Color::Reset,
            text_secondary: Color::DarkGray,
            text_accent: Color::Blue,
            project_colors: vec![
                (Color::Magenta, Color::LightMagenta),
                (Color::Cyan, Color::LightCyan),
                (Color::Blue, Color::LightBlue),
                (Color::Yellow, Color::LightYellow),
                (Color::Green, Color::LightGreen),
                (Color::Red, Color::LightRed),
            ],

            diff_added: Color::Green,
            diff_removed: Color::Red,
            diff_hunk_header: Color::Cyan,
            diff_file_header: Color::Yellow,
            diff_context: Color::Reset,

            modal_info: Color::Cyan,
            modal_warning: Color::Yellow,
            modal_error: Color::Red,

            palette_command_bg: Color::DarkGray,
            palette_command_fg: Color::White,

            status_bar_bg: Color::Blue,
            status_bar_fg: Color::White,
        }
    }

    /// 256-color theme (good balance of compatibility and aesthetics)
    pub fn indexed() -> Self {
        Self {
            border_focused: Color::Indexed(117), // Pastel sky blue
            border_unfocused: Color::Indexed(243),

            selection_bg: Color::Indexed(60), // Muted purple-blue
            selection_fg: Some(Color::Indexed(255)),

            status_creating: Color::Indexed(228), // Pastel yellow
            status_running: Color::Indexed(156),  // Pastel mint green
            status_stopped: Color::Indexed(248),
            status_pr: Color::Indexed(114),       // Pastel green
            status_pr_merged: Color::Indexed(97), // Dark purple

            pr_open: Color::Indexed(111),   // Pastel sky blue
            pr_draft: Color::Indexed(245),  // Mid-grey
            pr_closed: Color::Indexed(167), // Soft red

            // Darker variants readable with bold near-white text
            pr_pill_open_bg: Color::Indexed(24),    // Dark blue
            pr_pill_draft_bg: Color::Indexed(240),  // Dark grey
            pr_pill_closed_bg: Color::Indexed(124), // Dark red
            pr_pill_review_bg: Color::Indexed(22),  // Dark green
            pr_pill_merged_bg: Color::Indexed(54),  // Very dark purple
            pr_pill_text: Color::Indexed(231),      // Near-white

            agent_working: AgentWorkingStyle::Rainbow,
            agent_waiting: Color::Indexed(208),    // Orange
            unread_indicator: Color::Indexed(117), // Sky blue

            text_primary: Color::Reset,
            text_secondary: Color::Indexed(250),
            text_accent: Color::Indexed(147), // Pastel lavender
            project_colors: vec![
                (Color::Indexed(168), Color::Indexed(218)), // Pink
                (Color::Indexed(68), Color::Indexed(117)),  // Blue
                (Color::Indexed(71), Color::Indexed(157)),  // Green
                (Color::Indexed(173), Color::Indexed(222)), // Orange
                (Color::Indexed(134), Color::Indexed(183)), // Purple
                (Color::Indexed(73), Color::Indexed(152)),  // Teal
            ],

            diff_added: Color::Indexed(156),       // Pastel mint
            diff_removed: Color::Indexed(210),     // Pastel coral
            diff_hunk_header: Color::Indexed(183), // Pastel orchid
            diff_file_header: Color::Indexed(223), // Pastel cream
            diff_context: Color::Reset,

            modal_info: Color::Indexed(117),    // Pastel sky
            modal_warning: Color::Indexed(222), // Pastel peach
            modal_error: Color::Indexed(210),   // Pastel coral

            palette_command_bg: Color::Indexed(23), // Deep muted turquoise
            palette_command_fg: Color::Indexed(252), // Near-white

            status_bar_bg: Color::Indexed(236),
            status_bar_fg: Color::Indexed(252),
        }
    }

    /// True color theme (richest visual experience)
    pub fn truecolor() -> Self {
        Self {
            border_focused: Color::Rgb(137, 180, 250), // Pastel sky blue
            border_unfocused: Color::Rgb(88, 91, 112),

            selection_bg: Color::Rgb(69, 71, 90),
            selection_fg: Some(Color::Rgb(245, 245, 250)),

            status_creating: Color::Rgb(249, 240, 107), // Pastel yellow
            status_running: Color::Rgb(166, 227, 161),  // Pastel mint
            status_stopped: Color::Rgb(147, 153, 178),  // Muted lavender
            status_pr: Color::Rgb(126, 198, 153),       // Soft GitHub-ish green
            status_pr_merged: Color::Rgb(137, 100, 180), // Dark purple

            pr_open: Color::Rgb(137, 180, 250),  // Pastel sky blue
            pr_draft: Color::Rgb(147, 153, 178), // Muted grey-lavender
            pr_closed: Color::Rgb(243, 139, 168), // Pastel rose / soft red

            // Darker variants for pill backgrounds, readable with bold near-white text
            pr_pill_open_bg: Color::Rgb(40, 80, 160), // Dark blue
            pr_pill_draft_bg: Color::Rgb(80, 86, 110), // Dark slate
            pr_pill_closed_bg: Color::Rgb(160, 60, 85), // Dark rose
            pr_pill_review_bg: Color::Rgb(46, 125, 80), // Dark green
            pr_pill_merged_bg: Color::Rgb(75, 55, 110), // Very dark purple
            pr_pill_text: Color::Rgb(245, 245, 250),  // Near-white

            agent_working: AgentWorkingStyle::Rainbow,
            agent_waiting: Color::Rgb(250, 179, 135), // Peach/orange
            unread_indicator: Color::Rgb(137, 180, 250), // Sky blue

            text_primary: Color::Rgb(245, 245, 250),
            text_secondary: Color::Rgb(166, 173, 200),
            text_accent: Color::Rgb(180, 190, 254), // Pastel periwinkle
            project_colors: vec![
                (Color::Rgb(199, 120, 140), Color::Rgb(243, 174, 190)), // Pink
                (Color::Rgb(100, 140, 210), Color::Rgb(160, 190, 245)), // Blue
                (Color::Rgb(100, 165, 110), Color::Rgb(166, 218, 170)), // Green
                (Color::Rgb(210, 160, 100), Color::Rgb(245, 210, 165)), // Orange
                (Color::Rgb(160, 130, 200), Color::Rgb(200, 175, 240)), // Purple
                (Color::Rgb(90, 170, 170), Color::Rgb(155, 215, 215)),  // Teal
            ],

            diff_added: Color::Rgb(166, 227, 161), // Pastel mint
            diff_removed: Color::Rgb(243, 139, 168), // Pastel rose
            diff_hunk_header: Color::Rgb(203, 166, 247), // Pastel mauve
            diff_file_header: Color::Rgb(249, 226, 175), // Pastel peach
            diff_context: Color::Reset,

            modal_info: Color::Rgb(137, 180, 250), // Pastel sky
            modal_warning: Color::Rgb(249, 226, 175), // Pastel peach
            modal_error: Color::Rgb(243, 139, 168), // Pastel rose

            palette_command_bg: Color::Rgb(60, 88, 92), // Deep muted turquoise-teal
            palette_command_fg: Color::Rgb(205, 214, 244), // Near-white lavender

            status_bar_bg: Color::Rgb(49, 50, 68),
            status_bar_fg: Color::Rgb(205, 214, 244),
        }
    }

    /// Monokai Dimmed — a muted/desaturated take on the classic Monokai color scheme
    pub fn monokai_dimmed() -> Self {
        Self {
            border_focused: Color::Rgb(181, 165, 106), // Muted gold/yellow #b5a56a
            border_unfocused: Color::Rgb(85, 85, 85),  // Dark gray #555555

            selection_bg: Color::Rgb(58, 61, 65), // Dark blue-gray #3a3d41
            selection_fg: Some(Color::Rgb(255, 255, 255)),

            status_creating: Color::Rgb(220, 220, 170), // Muted yellow #dcdcaa
            status_running: Color::Rgb(181, 206, 168),  // Muted green #b5cea8
            status_stopped: Color::Rgb(128, 128, 128),  // Gray #808080
            status_pr: Color::Rgb(181, 206, 168),       // Muted green
            status_pr_merged: Color::Rgb(128, 128, 128), // Gray

            pr_open: Color::Rgb(103, 150, 230), // Muted blue #6796e6
            pr_draft: Color::Rgb(128, 128, 128), // Gray #808080
            pr_closed: Color::Rgb(209, 105, 105), // Muted red #d16969

            pr_pill_open_bg: Color::Rgb(45, 65, 120), // Dark muted blue
            pr_pill_draft_bg: Color::Rgb(70, 70, 70), // Dark gray
            pr_pill_closed_bg: Color::Rgb(120, 50, 50), // Dark muted red
            pr_pill_review_bg: Color::Rgb(55, 90, 55), // Dark muted green
            pr_pill_merged_bg: Color::Rgb(85, 60, 100), // Dark muted purple
            pr_pill_text: Color::Rgb(230, 230, 230),  // Near-white

            agent_working: AgentWorkingStyle::Rainbow,
            agent_waiting: Color::Rgb(220, 220, 170), // Muted yellow #dcdcaa
            unread_indicator: Color::Rgb(103, 150, 230), // Muted blue #6796e6

            text_primary: Color::Rgb(212, 212, 212), // Light gray #d4d4d4
            text_secondary: Color::Rgb(150, 150, 150), // Medium gray #969696
            text_accent: Color::Rgb(124, 165, 212),  // Muted blue #7ca5d4
            project_colors: vec![
                (Color::Rgb(181, 165, 106), Color::Rgb(220, 220, 170)), // Gold / muted yellow
                (Color::Rgb(100, 160, 160), Color::Rgb(140, 200, 200)), // Teal
                (Color::Rgb(206, 145, 120), Color::Rgb(230, 180, 160)), // Coral
                (Color::Rgb(106, 130, 180), Color::Rgb(150, 170, 210)), // Slate-blue
                (Color::Rgb(140, 170, 140), Color::Rgb(181, 206, 168)), // Sage
                (Color::Rgb(170, 140, 170), Color::Rgb(197, 134, 192)), // Mauve
            ],

            diff_added: Color::Rgb(181, 206, 168), // Muted green #b5cea8
            diff_removed: Color::Rgb(209, 105, 105), // Muted red #d16969
            diff_hunk_header: Color::Rgb(197, 134, 192), // Muted purple #c586c0
            diff_file_header: Color::Rgb(220, 220, 170), // Muted yellow #dcdcaa
            diff_context: Color::Reset,

            modal_info: Color::Rgb(124, 165, 212), // Muted blue #7ca5d4
            modal_warning: Color::Rgb(220, 220, 170), // Muted yellow #dcdcaa
            modal_error: Color::Rgb(209, 105, 105), // Muted red #d16969

            palette_command_bg: Color::Rgb(50, 50, 50), // Dark gray
            palette_command_fg: Color::Rgb(204, 204, 204), // Light gray #cccccc

            status_bar_bg: Color::Rgb(45, 45, 45), // Dark gray #2d2d2d
            status_bar_fg: Color::Rgb(204, 204, 204), // Light gray #cccccc
        }
    }

    /// Zedokai — inspired by the Zed editor's Monokai variant with a filter/spectrum twist
    pub fn zedokai() -> Self {
        Self {
            border_focused: Color::Rgb(249, 38, 114), // Vivid pink #f92672
            border_unfocused: Color::Rgb(73, 72, 62), // Dark gray #49483e

            selection_bg: Color::Rgb(73, 72, 62), // Dark warm gray #49483e
            selection_fg: Some(Color::Rgb(248, 248, 242)), // Bright white #f8f8f2

            status_creating: Color::Rgb(253, 151, 31), // Vivid orange #fd971f
            status_running: Color::Rgb(166, 226, 46),  // Bright green #a6e22e
            status_stopped: Color::Rgb(117, 113, 94),  // Warm gray #75715e
            status_pr: Color::Rgb(166, 226, 46),       // Bright green
            status_pr_merged: Color::Rgb(117, 113, 94), // Warm gray

            pr_open: Color::Rgb(102, 217, 239), // Sky blue #66d9ef
            pr_draft: Color::Rgb(117, 113, 94), // Warm gray #75715e
            pr_closed: Color::Rgb(249, 38, 114), // Pink #f92672

            pr_pill_open_bg: Color::Rgb(30, 90, 100), // Dark cyan
            pr_pill_draft_bg: Color::Rgb(55, 55, 48), // Dark warm gray
            pr_pill_closed_bg: Color::Rgb(120, 15, 55), // Dark pink
            pr_pill_review_bg: Color::Rgb(50, 100, 15), // Dark green
            pr_pill_merged_bg: Color::Rgb(70, 45, 100), // Dark purple
            pr_pill_text: Color::Rgb(248, 248, 242),  // Bright white #f8f8f2

            agent_working: AgentWorkingStyle::Rainbow,
            agent_waiting: Color::Rgb(253, 151, 31), // Orange #fd971f
            unread_indicator: Color::Rgb(102, 217, 239), // Sky blue #66d9ef

            text_primary: Color::Rgb(248, 248, 242), // Warm white #f8f8f2
            text_secondary: Color::Rgb(117, 113, 94), // Warm gray #75715e
            text_accent: Color::Rgb(174, 129, 255),  // Vivid purple #ae81ff
            project_colors: vec![
                (Color::Rgb(249, 38, 114), Color::Rgb(255, 100, 150)), // Pink
                (Color::Rgb(102, 217, 239), Color::Rgb(140, 230, 245)), // Cyan
                (Color::Rgb(166, 226, 46), Color::Rgb(200, 240, 110)), // Green
                (Color::Rgb(253, 151, 31), Color::Rgb(255, 190, 100)), // Orange
                (Color::Rgb(174, 129, 255), Color::Rgb(200, 170, 255)), // Purple
                (Color::Rgb(230, 219, 116), Color::Rgb(245, 235, 160)), // Yellow
            ],

            diff_added: Color::Rgb(166, 226, 46), // Green #a6e22e
            diff_removed: Color::Rgb(249, 38, 114), // Pink #f92672
            diff_hunk_header: Color::Rgb(102, 217, 239), // Blue #66d9ef
            diff_file_header: Color::Rgb(230, 219, 116), // Yellow #e6db74
            diff_context: Color::Reset,

            modal_info: Color::Rgb(102, 217, 239), // Cyan #66d9ef
            modal_warning: Color::Rgb(253, 151, 31), // Orange #fd971f
            modal_error: Color::Rgb(249, 38, 114), // Pink #f92672

            palette_command_bg: Color::Rgb(50, 50, 42), // Dark warm gray
            palette_command_fg: Color::Rgb(248, 248, 242), // Warm white #f8f8f2

            status_bar_bg: Color::Rgb(30, 31, 28), // Very dark bg #1e1f1c
            status_bar_fg: Color::Rgb(248, 248, 242), // Warm white #f8f8f2
        }
    }

    /// Rosé Pine — a soft pink/rose aesthetic inspired by Rosé Pine
    pub fn rose_pine() -> Self {
        Self {
            border_focused: Color::Rgb(235, 111, 146), // Rose/pink #eb6f92
            border_unfocused: Color::Rgb(57, 53, 82),  // Muted overlay #393552

            selection_bg: Color::Rgb(42, 39, 63), // Subtle highlight #2a273f
            selection_fg: Some(Color::Rgb(224, 222, 244)), // Soft white #e0def4

            status_creating: Color::Rgb(246, 193, 119), // Gold #f6c177
            status_running: Color::Rgb(156, 207, 216),  // Foam/teal #9ccfd8
            status_stopped: Color::Rgb(110, 106, 134),  // Muted #6e6a86
            status_pr: Color::Rgb(156, 207, 216),       // Foam/teal
            status_pr_merged: Color::Rgb(110, 106, 134), // Muted

            pr_open: Color::Rgb(196, 167, 231), // Iris/purple #c4a7e7
            pr_draft: Color::Rgb(110, 106, 134), // Muted #6e6a86
            pr_closed: Color::Rgb(235, 111, 146), // Love/pink #eb6f92

            pr_pill_open_bg: Color::Rgb(80, 60, 110), // Dark iris
            pr_pill_draft_bg: Color::Rgb(55, 53, 70), // Dark muted
            pr_pill_closed_bg: Color::Rgb(110, 45, 65), // Dark love
            pr_pill_review_bg: Color::Rgb(40, 90, 95), // Dark foam
            pr_pill_merged_bg: Color::Rgb(50, 40, 75), // Dark iris variant
            pr_pill_text: Color::Rgb(224, 222, 244),  // Soft white #e0def4

            agent_working: AgentWorkingStyle::Rainbow,
            agent_waiting: Color::Rgb(246, 193, 119), // Gold #f6c177
            unread_indicator: Color::Rgb(196, 167, 231), // Iris #c4a7e7

            text_primary: Color::Rgb(224, 222, 244), // Soft white #e0def4
            text_secondary: Color::Rgb(144, 140, 170), // Subtle #908caa
            text_accent: Color::Rgb(196, 167, 231),  // Iris/purple #c4a7e7
            project_colors: vec![
                (Color::Rgb(235, 111, 146), Color::Rgb(235, 188, 186)), // Rose / rose-lighter
                (Color::Rgb(196, 167, 231), Color::Rgb(196, 167, 231)), // Iris
                (Color::Rgb(156, 207, 216), Color::Rgb(156, 207, 216)), // Foam
                (Color::Rgb(246, 193, 119), Color::Rgb(246, 193, 119)), // Gold
                (Color::Rgb(49, 116, 143), Color::Rgb(62, 143, 176)),   // Pine
                (Color::Rgb(235, 111, 146), Color::Rgb(215, 130, 126)), // Love variant
            ],

            diff_added: Color::Rgb(156, 207, 216), // Foam #9ccfd8
            diff_removed: Color::Rgb(235, 111, 146), // Love/pink #eb6f92
            diff_hunk_header: Color::Rgb(196, 167, 231), // Iris #c4a7e7
            diff_file_header: Color::Rgb(246, 193, 119), // Gold #f6c177
            diff_context: Color::Reset,

            modal_info: Color::Rgb(156, 207, 216), // Foam #9ccfd8
            modal_warning: Color::Rgb(246, 193, 119), // Gold #f6c177
            modal_error: Color::Rgb(235, 111, 146), // Love #eb6f92

            palette_command_bg: Color::Rgb(38, 35, 58), // Dark navy-rose #26233a
            palette_command_fg: Color::Rgb(224, 222, 244), // Soft white #e0def4

            status_bar_bg: Color::Rgb(31, 29, 46), // Dark bg #1f1d2e
            status_bar_fg: Color::Rgb(224, 222, 244), // Muted rose fg #e0def4
        }
    }

    /// Look up a preset palette by name.
    ///
    /// Recognised names: `"basic"`, `"indexed"`, `"truecolor"`, `"monokai-dimmed"`,
    /// `"zedokai"`, `"rosé-pine"` / `"rose-pine"`.
    pub fn from_preset(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "basic" => Some(Self::basic()),
            "indexed" => Some(Self::indexed()),
            "truecolor" => Some(Self::truecolor()),
            "monokai-dimmed" | "monokai_dimmed" => Some(Self::monokai_dimmed()),
            "zedokai" => Some(Self::zedokai()),
            "rosé-pine" | "rose-pine" | "rosé_pine" | "rose_pine" => Some(Self::rose_pine()),
            _ => None,
        }
    }

    /// Apply user-supplied overrides on top of this theme.
    ///
    /// Only `Some` fields in `overrides` replace the corresponding color;
    /// `None` fields leave the base theme value untouched.
    pub fn with_overrides(mut self, overrides: &ThemeOverrides) -> Self {
        macro_rules! apply {
            ($field:ident) => {
                if let Some(cv) = overrides.$field {
                    self.$field = cv.0;
                }
            };
        }

        apply!(border_focused);
        apply!(border_unfocused);
        apply!(selection_bg);
        apply!(status_creating);
        apply!(status_running);
        apply!(status_stopped);
        apply!(status_pr);
        apply!(status_pr_merged);
        apply!(pr_open);
        apply!(pr_draft);
        apply!(pr_closed);
        apply!(pr_pill_open_bg);
        apply!(pr_pill_draft_bg);
        apply!(pr_pill_closed_bg);
        apply!(pr_pill_review_bg);
        apply!(pr_pill_merged_bg);
        apply!(pr_pill_text);
        apply!(agent_waiting);
        apply!(unread_indicator);
        apply!(text_primary);
        apply!(text_secondary);
        apply!(text_accent);
        apply!(diff_added);
        apply!(diff_removed);
        apply!(diff_hunk_header);
        apply!(diff_file_header);
        apply!(diff_context);
        apply!(modal_info);
        apply!(modal_warning);
        apply!(modal_error);
        apply!(palette_command_bg);
        apply!(palette_command_fg);
        apply!(status_bar_bg);
        apply!(status_bar_fg);

        // selection_fg is Option<Color> in Theme but Option<ColorValue> in overrides
        if let Some(cv) = overrides.selection_fg {
            self.selection_fg = Some(cv.0);
        }

        // agent_working uses AgentWorkingStyle, not ColorValue, so it's applied
        // directly without unwrapping a `.0`.
        if let Some(style) = overrides.agent_working {
            self.agent_working = style;
        }

        // project_colors is intentionally not overridable — paired-tuple
        // arrays are ergonomically poor in TOML and the feature has minimal
        // user demand.

        self
    }

    /// Style for focused pane borders
    pub fn border_focused(&self) -> Style {
        Style::default().fg(self.border_focused)
    }

    /// Style for unfocused pane borders
    pub fn border_unfocused(&self) -> Style {
        Style::default().fg(self.border_unfocused)
    }

    /// Style for selected items
    pub fn selection(&self) -> Style {
        let style = Style::default().bg(self.selection_bg);
        match self.selection_fg {
            Some(fg) => style.fg(fg),
            None => style,
        }
    }

    /// Get project colors by index (cycles through palette)
    pub fn project_color(&self, index: usize) -> (Color, Color) {
        self.project_colors[index % self.project_colors.len()]
    }

    /// Style for status bar
    pub fn status_bar(&self) -> Style {
        Style::default()
            .bg(self.status_bar_bg)
            .fg(self.status_bar_fg)
    }

    /// Return a tmux-compatible `status-style` string matching this theme's status bar colors
    pub fn tmux_status_style(&self) -> String {
        format!(
            "bg={},fg={}",
            color_to_tmux(self.status_bar_bg),
            color_to_tmux(self.status_bar_fg),
        )
    }
}

/// Scale a color's brightness toward black by the given factor (0.0 = black, 1.0 = unchanged).
///
/// For named and indexed colors that can't be scaled directly, falls back to the
/// closest indexed gray from the 256-color palette.
pub fn dim_color(color: Color, opacity: f32) -> Color {
    let opacity = opacity.clamp(0.0, 1.0);
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as f32 * opacity) as u8,
            (g as f32 * opacity) as u8,
            (b as f32 * opacity) as u8,
        ),
        Color::Reset => {
            // Reset means "terminal default" — dim to a gray proportional to opacity
            // Assume default text is ~200 brightness
            let v = (200.0 * opacity) as u8;
            Color::Rgb(v, v, v)
        }
        other => {
            // Convert named/indexed colors to approximate RGB, then dim
            let (r, g, b) = color_to_approx_rgb(other);
            Color::Rgb(
                (r as f32 * opacity) as u8,
                (g as f32 * opacity) as u8,
                (b as f32 * opacity) as u8,
            )
        }
    }
}

/// Approximate RGB values for named ANSI colors
fn color_to_approx_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Black => (0, 0, 0),
        Color::Red => (205, 0, 0),
        Color::Green => (0, 205, 0),
        Color::Yellow => (205, 205, 0),
        Color::Blue => (0, 0, 238),
        Color::Magenta => (205, 0, 205),
        Color::Cyan => (0, 205, 205),
        Color::White | Color::Gray => (229, 229, 229),
        Color::DarkGray => (127, 127, 127),
        Color::LightRed => (255, 0, 0),
        Color::LightGreen => (0, 255, 0),
        Color::LightYellow => (255, 255, 0),
        Color::LightBlue => (92, 92, 255),
        Color::LightMagenta => (255, 0, 255),
        Color::LightCyan => (0, 255, 255),
        Color::Indexed(n) => indexed_to_rgb(n),
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Reset => (200, 200, 200),
    }
}

/// Convert a 256-color index to approximate RGB
fn indexed_to_rgb(n: u8) -> (u8, u8, u8) {
    match n {
        // Standard 16 colors — delegate to named
        0 => (0, 0, 0),
        1 => (205, 0, 0),
        2 => (0, 205, 0),
        3 => (205, 205, 0),
        4 => (0, 0, 238),
        5 => (205, 0, 205),
        6 => (0, 205, 205),
        7 => (229, 229, 229),
        8 => (127, 127, 127),
        9 => (255, 0, 0),
        10 => (0, 255, 0),
        11 => (255, 255, 0),
        12 => (92, 92, 255),
        13 => (255, 0, 255),
        14 => (0, 255, 255),
        15 => (255, 255, 255),
        // 6x6x6 color cube (indices 16-231)
        16..=231 => {
            let n = n - 16;
            let b = n % 6;
            let g = (n / 6) % 6;
            let r = n / 36;
            let to_val = |c: u8| if c == 0 { 0u8 } else { 55 + 40 * c };
            (to_val(r), to_val(g), to_val(b))
        }
        // Grayscale ramp (indices 232-255)
        232..=255 => {
            let v = 8 + 10 * (n - 232);
            (v, v, v)
        }
    }
}

/// Convert a ratatui `Color` to a tmux-compatible color string
pub fn color_to_tmux(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),
        Color::Indexed(n) => format!("colour{}", n),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::White | Color::Gray => "white".into(),
        Color::DarkGray => "brightblack".into(),
        Color::LightRed => "brightred".into(),
        Color::LightGreen => "brightgreen".into(),
        Color::LightYellow => "brightyellow".into(),
        Color::LightBlue => "brightblue".into(),
        Color::LightMagenta => "brightmagenta".into(),
        Color::LightCyan => "brightcyan".into(),
        Color::Reset => "default".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::theme::{ColorValue, ThemeOverrides};

    #[test]
    fn test_basic_theme() {
        let theme = Theme::basic();
        assert_eq!(theme.border_focused, Color::Cyan);
        assert_eq!(theme.status_running, Color::Green);
    }

    #[test]
    fn test_indexed_theme() {
        let theme = Theme::indexed();
        assert_eq!(theme.border_focused, Color::Indexed(117));
        assert_eq!(theme.selection_bg, Color::Indexed(60));
    }

    #[test]
    fn test_truecolor_theme() {
        let theme = Theme::truecolor();
        assert_eq!(theme.border_focused, Color::Rgb(137, 180, 250));
        assert_eq!(theme.status_running, Color::Rgb(166, 227, 161));
    }

    #[test]
    fn test_theme_styles() {
        let theme = Theme::basic();
        let style = theme.border_focused();
        assert_eq!(style.fg, Some(Color::Cyan));
    }

    #[test]
    fn test_selection_style() {
        let theme = Theme::indexed();
        let style = theme.selection();
        assert_eq!(style.bg, Some(Color::Indexed(60)));
        assert_eq!(style.fg, Some(Color::Indexed(255)));
    }

    #[test]
    fn test_color_mode_for_theme() {
        let basic = Theme::for_color_mode(ColorMode::Basic);
        let indexed = Theme::for_color_mode(ColorMode::Indexed);
        let truecolor = Theme::for_color_mode(ColorMode::TrueColor);

        assert_eq!(basic.border_focused, Color::Cyan);
        assert_eq!(indexed.border_focused, Color::Indexed(117));
        assert_eq!(truecolor.border_focused, Color::Rgb(137, 180, 250));
    }

    #[test]
    fn test_from_preset_valid() {
        assert_eq!(
            Theme::from_preset("basic").unwrap().border_focused,
            Color::Cyan
        );
        assert_eq!(
            Theme::from_preset("indexed").unwrap().border_focused,
            Color::Indexed(117)
        );
        assert_eq!(
            Theme::from_preset("TrueColor").unwrap().border_focused,
            Color::Rgb(137, 180, 250)
        );
    }

    #[test]
    fn test_from_preset_monokai_dimmed() {
        let theme = Theme::from_preset("monokai-dimmed").unwrap();
        assert_eq!(theme.border_focused, Color::Rgb(181, 165, 106));
        assert_eq!(theme.status_running, Color::Rgb(181, 206, 168));
        assert_eq!(theme.text_primary, Color::Rgb(212, 212, 212));
        assert_eq!(theme.project_colors.len(), 6);

        // Underscore variant
        assert!(Theme::from_preset("monokai_dimmed").is_some());
    }

    #[test]
    fn test_from_preset_zedokai() {
        let theme = Theme::from_preset("zedokai").unwrap();
        assert_eq!(theme.border_focused, Color::Rgb(249, 38, 114));
        assert_eq!(theme.status_running, Color::Rgb(166, 226, 46));
        assert_eq!(theme.text_primary, Color::Rgb(248, 248, 242));
        assert_eq!(theme.project_colors.len(), 6);
    }

    #[test]
    fn test_from_preset_rose_pine() {
        let theme = Theme::from_preset("rose-pine").unwrap();
        assert_eq!(theme.border_focused, Color::Rgb(235, 111, 146));
        assert_eq!(theme.status_running, Color::Rgb(156, 207, 216));
        assert_eq!(theme.text_primary, Color::Rgb(224, 222, 244));
        assert_eq!(theme.project_colors.len(), 6);

        // Alternate name variants all resolve
        assert!(Theme::from_preset("rosé-pine").is_some());
        assert!(Theme::from_preset("rose_pine").is_some());
        assert!(Theme::from_preset("rosé_pine").is_some());
    }

    #[test]
    fn test_from_preset_unknown_returns_none() {
        assert!(Theme::from_preset("catppuccin").is_none());
    }

    #[test]
    fn test_with_overrides_applies_some_fields() {
        let base = Theme::basic();
        let overrides = ThemeOverrides {
            border_focused: Some(ColorValue(Color::Rgb(255, 0, 0))),
            status_running: Some(ColorValue(Color::Yellow)),
            ..Default::default()
        };
        let themed = base.with_overrides(&overrides);
        assert_eq!(themed.border_focused, Color::Rgb(255, 0, 0));
        assert_eq!(themed.status_running, Color::Yellow);
        // Untouched fields keep the base value
        assert_eq!(themed.border_unfocused, Color::DarkGray);
        assert_eq!(themed.status_stopped, Color::DarkGray);
    }

    #[test]
    fn test_with_overrides_empty_is_identity() {
        let base = Theme::indexed();
        let themed = base.clone().with_overrides(&ThemeOverrides::default());
        assert_eq!(themed.border_focused, base.border_focused);
        assert_eq!(themed.selection_bg, base.selection_bg);
        assert_eq!(themed.status_bar_bg, base.status_bar_bg);
    }

    #[test]
    fn test_with_overrides_selection_fg() {
        let base = Theme::basic();
        let overrides = ThemeOverrides {
            selection_fg: Some(ColorValue(Color::Rgb(1, 2, 3))),
            ..Default::default()
        };
        let themed = base.with_overrides(&overrides);
        assert_eq!(themed.selection_fg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn test_color_to_tmux_rgb() {
        assert_eq!(color_to_tmux(Color::Rgb(49, 50, 68)), "#313244");
        assert_eq!(color_to_tmux(Color::Rgb(0, 0, 0)), "#000000");
        assert_eq!(color_to_tmux(Color::Rgb(255, 255, 255)), "#ffffff");
    }

    #[test]
    fn test_color_to_tmux_indexed() {
        assert_eq!(color_to_tmux(Color::Indexed(236)), "colour236");
        assert_eq!(color_to_tmux(Color::Indexed(0)), "colour0");
    }

    #[test]
    fn test_color_to_tmux_named() {
        assert_eq!(color_to_tmux(Color::Blue), "blue");
        assert_eq!(color_to_tmux(Color::White), "white");
        assert_eq!(color_to_tmux(Color::DarkGray), "brightblack");
        assert_eq!(color_to_tmux(Color::Reset), "default");
    }

    #[test]
    fn test_dim_color_rgb() {
        // 50% opacity halves each channel
        assert_eq!(
            dim_color(Color::Rgb(200, 100, 50), 0.5),
            Color::Rgb(100, 50, 25)
        );
    }

    #[test]
    fn test_dim_color_full_opacity_unchanged() {
        assert_eq!(
            dim_color(Color::Rgb(200, 100, 50), 1.0),
            Color::Rgb(200, 100, 50)
        );
    }

    #[test]
    fn test_dim_color_zero_opacity_is_black() {
        assert_eq!(
            dim_color(Color::Rgb(200, 100, 50), 0.0),
            Color::Rgb(0, 0, 0)
        );
    }

    #[test]
    fn test_dim_color_named_converts_to_rgb() {
        // Green at 50% should be approximately half brightness
        let dimmed = dim_color(Color::Green, 0.5);
        assert!(matches!(dimmed, Color::Rgb(_, _, _)));
    }

    #[test]
    fn test_dim_color_indexed_converts_to_rgb() {
        let dimmed = dim_color(Color::Indexed(196), 0.5);
        assert!(matches!(dimmed, Color::Rgb(_, _, _)));
    }

    #[test]
    fn test_dim_color_reset_produces_gray() {
        let dimmed = dim_color(Color::Reset, 0.5);
        assert_eq!(dimmed, Color::Rgb(100, 100, 100));
    }

    #[test]
    fn test_dim_color_clamps_opacity() {
        // Opacity > 1.0 should be clamped to 1.0
        assert_eq!(
            dim_color(Color::Rgb(200, 100, 50), 2.0),
            Color::Rgb(200, 100, 50)
        );
        // Opacity < 0.0 should be clamped to 0.0
        assert_eq!(
            dim_color(Color::Rgb(200, 100, 50), -1.0),
            Color::Rgb(0, 0, 0)
        );
    }

    #[test]
    fn test_indexed_to_rgb_grayscale_ramp() {
        // Index 232 = darkest gray (8)
        assert_eq!(indexed_to_rgb(232), (8, 8, 8));
        // Index 255 = lightest gray (238)
        assert_eq!(indexed_to_rgb(255), (238, 238, 238));
    }

    #[test]
    fn test_indexed_to_rgb_color_cube() {
        // Index 16 = (0,0,0) in the 6x6x6 cube
        assert_eq!(indexed_to_rgb(16), (0, 0, 0));
        // Index 196 = (5,0,0) = bright red
        assert_eq!(indexed_to_rgb(196), (255, 0, 0));
    }

    #[test]
    fn test_tmux_status_style_per_theme() {
        assert_eq!(Theme::basic().tmux_status_style(), "bg=blue,fg=white");
        assert_eq!(
            Theme::indexed().tmux_status_style(),
            "bg=colour236,fg=colour252"
        );
        assert_eq!(
            Theme::truecolor().tmux_status_style(),
            "bg=#313244,fg=#cdd6f4"
        );
    }
}
