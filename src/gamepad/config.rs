//! Mapping table for the virtual gamepad: which remote button drives which
//! virtual control, plus the stick-shaping knobs.
//!
//! Precedence, lowest to highest: the built-in defaults in this module, the
//! optional `gamepad.toml` under the shared config directory (see
//! [`crate::calibration::config_file_in`]), then the CLI flags on
//! [`crate::cli::GamepadArgs`]. Every file field is optional, so a partial
//! file overrides only what it names; unknown keys are a hard error rather
//! than a silent no-op.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use evdev::KeyCode;
use serde::Deserialize;

/// How the touchpad drives the left stick. Selected once at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum StickMode {
    /// Touch-down point becomes stick centre; displacement deflects the stick.
    Relative,
    /// Calibrated pad position maps straight onto the stick.
    Absolute,
}

/// One D-pad direction on `ABS_HAT0X` / `ABS_HAT0Y`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HatDir {
    Up,
    Down,
    Left,
    Right,
}

/// Where one remote button lands on the virtual pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Key(KeyCode),
    Hat(HatDir),
    None,
}

/// (mask bit, remote button config key, default target).
/// Bit values and labels match `crate::decoder::BUTTON_NAMES`.
///
/// The kernel names the face buttons by compass direction:
/// `BTN_NORTH == BTN_X` and `BTN_WEST == BTN_Y`, so the X / Y comments
/// below are correct despite the names.
pub const DEFAULT_BUTTONS: [(u16, &str, Target); 13] = [
    (0x0001, "tv", Target::Key(KeyCode::BTN_MODE)), // guide
    (0x0002, "volume_up", Target::Key(KeyCode::BTN_TR)),
    (0x0004, "volume_down", Target::Key(KeyCode::BTN_TL)),
    (0x0008, "select", Target::Key(KeyCode::BTN_SOUTH)), // A
    (0x0010, "power", Target::Key(KeyCode::BTN_SELECT)), // back
    (0x0020, "siri", Target::Key(KeyCode::BTN_NORTH)),   // X
    (0x0040, "back", Target::Key(KeyCode::BTN_EAST)),    // B
    (0x0080, "mute", Target::Key(KeyCode::BTN_WEST)),    // Y
    (0x0100, "play_pause", Target::Key(KeyCode::BTN_START)),
    (0x0200, "up", Target::Hat(HatDir::Up)),
    (0x0400, "right", Target::Hat(HatDir::Right)),
    (0x0800, "down", Target::Hat(HatDir::Down)),
    (0x1000, "left", Target::Hat(HatDir::Left)),
];

pub const DEFAULT_STICK_MODE: StickMode = StickMode::Relative;
pub const DEFAULT_STICK_RADIUS: f64 = 0.35;
pub const DEFAULT_DEADZONE: f64 = 0.05;

/// Filename under the shared config directory.
pub const CONFIG_FILE: &str = "gamepad.toml";

/// Fully resolved runtime configuration.
#[derive(Clone, Debug)]
pub struct GamepadConfig {
    pub stick_mode: StickMode,
    pub stick_radius: f64,
    pub deadzone: f64,
    /// (mask bit, target), one entry per assigned bit of report 0xFB.
    pub buttons: [(u16, Target); 13],
}

/// On-disk schema. Every field is optional; unknown fields are rejected so a
/// typo surfaces instead of silently doing nothing.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub stick_mode: Option<StickMode>,
    pub stick_radius: Option<f64>,
    pub deadzone: Option<f64>,
    #[serde(default)]
    pub buttons: FileButtons,
}

/// `[buttons]` table: one optional target name per remote button.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileButtons {
    pub tv: Option<String>,
    pub volume_up: Option<String>,
    pub volume_down: Option<String>,
    pub select: Option<String>,
    pub power: Option<String>,
    pub siri: Option<String>,
    pub back: Option<String>,
    pub mute: Option<String>,
    pub play_pause: Option<String>,
    pub up: Option<String>,
    pub right: Option<String>,
    pub down: Option<String>,
    pub left: Option<String>,
}

impl FileButtons {
    /// Look up one entry by its [`DEFAULT_BUTTONS`] config key. Returns
    /// `None` both for an unset field and for a key that is not a remote
    /// button (the latter cannot happen: callers iterate `DEFAULT_BUTTONS`).
    pub fn get(&self, key: &str) -> Option<&str> {
        let field = match key {
            "tv" => &self.tv,
            "volume_up" => &self.volume_up,
            "volume_down" => &self.volume_down,
            "select" => &self.select,
            "power" => &self.power,
            "siri" => &self.siri,
            "back" => &self.back,
            "mute" => &self.mute,
            "play_pause" => &self.play_pause,
            "up" => &self.up,
            "right" => &self.right,
            "down" => &self.down,
            "left" => &self.left,
            _ => return None,
        };
        field.as_deref()
    }
}

/// Parse a config value into a [`Target`]. Accepts `none`, `HAT_UP` /
/// `HAT_DOWN` / `HAT_LEFT` / `HAT_RIGHT`, and the eleven buttons the
/// virtual pad declares. `BTN_A` / `BTN_B` / `BTN_X` / `BTN_Y` are
/// accepted as the kernel's own aliases for
/// `BTN_SOUTH` / `BTN_EAST` / `BTN_NORTH` / `BTN_WEST`.
///
/// Names outside that set are rejected: the kernel would silently drop
/// events for a code the device never declared.
pub fn parse_target(s: &str) -> Result<Target> {
    let upper = s.trim().to_ascii_uppercase();
    Ok(match upper.as_str() {
        "NONE" => Target::None,
        "HAT_UP" => Target::Hat(HatDir::Up),
        "HAT_DOWN" => Target::Hat(HatDir::Down),
        "HAT_LEFT" => Target::Hat(HatDir::Left),
        "HAT_RIGHT" => Target::Hat(HatDir::Right),
        "BTN_SOUTH" | "BTN_A" => Target::Key(KeyCode::BTN_SOUTH),
        "BTN_EAST" | "BTN_B" => Target::Key(KeyCode::BTN_EAST),
        "BTN_NORTH" | "BTN_X" => Target::Key(KeyCode::BTN_NORTH),
        "BTN_WEST" | "BTN_Y" => Target::Key(KeyCode::BTN_WEST),
        "BTN_TL" => Target::Key(KeyCode::BTN_TL),
        "BTN_TR" => Target::Key(KeyCode::BTN_TR),
        "BTN_SELECT" => Target::Key(KeyCode::BTN_SELECT),
        "BTN_START" => Target::Key(KeyCode::BTN_START),
        "BTN_MODE" => Target::Key(KeyCode::BTN_MODE),
        "BTN_THUMBL" => Target::Key(KeyCode::BTN_THUMBL),
        "BTN_THUMBR" => Target::Key(KeyCode::BTN_THUMBR),
        _ => bail!(
            "unknown gamepad target {s:?}; accepted: none, HAT_UP, HAT_DOWN, HAT_LEFT, \
             HAT_RIGHT, BTN_SOUTH (BTN_A), BTN_EAST (BTN_B), BTN_NORTH (BTN_X), \
             BTN_WEST (BTN_Y), BTN_TL, BTN_TR, BTN_SELECT, BTN_START, BTN_MODE, \
             BTN_THUMBL, BTN_THUMBR"
        ),
    })
}

/// Parse a mapping file. A missing file is the caller's problem (see
/// [`crate::gamepad::run`]) so the error stays distinguishable.
pub fn load_from(path: &Path) -> Result<FileConfig> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Default location, `<config base>/siri-remote/gamepad.toml`, or `None`
/// when neither `XDG_CONFIG_HOME` nor `HOME` is set.
pub fn default_path() -> Option<PathBuf> {
    Some(crate::calibration::config_file_in(
        &crate::calibration::config_base()?,
        CONFIG_FILE,
    ))
}

/// Fold CLI overrides over file values over built-in defaults, then validate.
///
/// Two remote buttons mapping to the same [`KeyCode`] is legal; the
/// translator ORs them together.
pub fn resolve(args: &crate::cli::GamepadArgs, file: &FileConfig) -> Result<GamepadConfig> {
    let stick_mode = args
        .stick_mode
        .or(file.stick_mode)
        .unwrap_or(DEFAULT_STICK_MODE);
    let stick_radius = args
        .stick_radius
        .or(file.stick_radius)
        .unwrap_or(DEFAULT_STICK_RADIUS);
    let deadzone = args.deadzone.or(file.deadzone).unwrap_or(DEFAULT_DEADZONE);

    if !(stick_radius > 0.0 && stick_radius <= 2.0) {
        bail!("stick_radius must be in (0.0, 2.0]; got {stick_radius}");
    }
    if !(0.0..0.9).contains(&deadzone) {
        bail!("deadzone must be in [0.0, 0.9); got {deadzone}");
    }

    let mut buttons = [(0u16, Target::None); 13];
    for (slot, (bit, key, default)) in buttons.iter_mut().zip(DEFAULT_BUTTONS) {
        let target = match file.buttons.get(key) {
            Some(name) => {
                parse_target(name).with_context(|| format!("gamepad button mapping for {key:?}"))?
            }
            None => default,
        };
        *slot = (bit, target);
    }

    Ok(GamepadConfig {
        stick_mode,
        stick_radius,
        deadzone,
        buttons,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::GamepadArgs;

    fn args() -> GamepadArgs {
        GamepadArgs {
            address: None,
            scan_seconds: 5.0,
            reconnect_delay: 0.5,
            stick_mode: None,
            stick_radius: None,
            deadzone: None,
            config: None,
        }
    }

    fn target_for(cfg: &GamepadConfig, bit: u16) -> Target {
        cfg.buttons
            .iter()
            .find(|(b, _)| *b == bit)
            .map(|(_, t)| *t)
            .expect("bit present in resolved config")
    }

    #[test]
    fn file_values_override_defaults() {
        let file: FileConfig = toml::from_str(
            "stick_mode = \"absolute\"\n\
             [buttons]\n\
             mute = \"BTN_THUMBL\"\n",
        )
        .unwrap();
        let cfg = resolve(&args(), &file).unwrap();
        assert_eq!(cfg.stick_mode, StickMode::Absolute);
        assert_eq!(target_for(&cfg, 0x0080), Target::Key(KeyCode::BTN_THUMBL));
        // Everything else keeps its built-in target.
        for (bit, _, default) in DEFAULT_BUTTONS {
            if bit == 0x0080 {
                continue;
            }
            assert_eq!(target_for(&cfg, bit), default, "bit {bit:#06x}");
        }
        assert_eq!(cfg.stick_radius, DEFAULT_STICK_RADIUS);
        assert_eq!(cfg.deadzone, DEFAULT_DEADZONE);
    }

    #[test]
    fn cli_beats_file() {
        let file: FileConfig = toml::from_str("stick_mode = \"absolute\"\n").unwrap();
        let mut a = args();
        a.stick_mode = Some(StickMode::Relative);
        assert_eq!(resolve(&a, &file).unwrap().stick_mode, StickMode::Relative);
    }

    #[test]
    fn aliases_and_case_fold_to_same_target() {
        assert_eq!(
            parse_target("btn_a").unwrap(),
            Target::Key(KeyCode::BTN_SOUTH)
        );
        assert_eq!(
            parse_target("BTN_SOUTH").unwrap(),
            Target::Key(KeyCode::BTN_SOUTH)
        );
        assert_eq!(parse_target("none").unwrap(), Target::None);
        assert_eq!(parse_target("hat_left").unwrap(), Target::Hat(HatDir::Left));
    }

    #[test]
    fn undeclared_button_name_is_rejected() {
        let err = parse_target("BTN_TRIGGER").unwrap_err().to_string();
        assert!(err.contains("BTN_TRIGGER"), "{err}");
        assert!(err.contains("BTN_SOUTH"), "{err}");
    }

    #[test]
    fn unknown_file_key_is_err() {
        assert!(
            toml::from_str::<FileConfig>("stick_radius = 0.4\nstikc_mode = \"absolute\"\n")
                .is_err()
        );
    }

    #[test]
    fn out_of_range_shaping_is_rejected() {
        let file = FileConfig {
            stick_radius: Some(0.0),
            ..Default::default()
        };
        assert!(resolve(&args(), &file).is_err());
        let file = FileConfig {
            deadzone: Some(0.95),
            ..Default::default()
        };
        assert!(resolve(&args(), &file).is_err());
    }
}
