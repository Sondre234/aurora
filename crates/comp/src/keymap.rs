use std::path::PathBuf;

use smithay::input::keyboard::XkbConfig;

use crate::state::Aurora;

/// Layout fields parsed from an xorg.conf.d keyboard snippet.
#[derive(Debug, Default, PartialEq)]
struct XorgKeyboard {
    model: String,
    layout: String,
    variant: String,
    options: String,
}

impl Aurora {
    /// Applies the keymap: keymap file, then `[input.keyboard]` in config.toml, then
    /// XKB_DEFAULT_*, then the system xorg config, then plain us. A source that fails to
    /// compile falls through.
    pub fn apply_keymap(&mut self) {
        let keyboard = self.keyboard.clone();

        if let Some(path) = keymap_file_path() {
            match std::fs::read_to_string(&path) {
                Ok(text) => match keyboard.set_keymap_from_string(self, text) {
                    Ok(()) => return tracing::info!(path = %path.display(), "keymap from file"),
                    Err(err) => {
                        tracing::warn!(path = %path.display(), ?err, "keymap file rejected")
                    }
                },
                // A missing default file is normal; a missing explicit one is not.
                Err(err) if std::env::var_os("AURORA_XKB_FILE").is_some() => {
                    tracing::warn!(path = %path.display(), %err, "cannot read AURORA_XKB_FILE")
                }
                Err(_) => {}
            }
        }

        let conf = self.config.input.keyboard.clone();
        if conf.has_xkb() {
            let field = |f: &Option<String>| f.clone().unwrap_or_default();
            let (rules, model) = (field(&conf.rules), field(&conf.model));
            let (layout, variant) = (field(&conf.layout), field(&conf.variant));
            let config = XkbConfig {
                rules: &rules,
                model: &model,
                layout: &layout,
                variant: &variant,
                options: conf.options.clone(),
            };
            match keyboard.set_xkb_config(self, config) {
                Ok(()) => {
                    return tracing::info!(
                        rules,
                        model,
                        layout,
                        variant,
                        options = conf.options.as_deref().unwrap_or_default(),
                        "keymap from [input.keyboard]"
                    );
                }
                Err(err) => tracing::warn!(?conf, ?err, "[input.keyboard] keymap rejected"),
            }
        }

        if std::env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("XKB_DEFAULT_")) {
            // Empty fields make xkbcommon read XKB_DEFAULT_* itself.
            match keyboard.set_xkb_config(self, XkbConfig::default()) {
                Ok(()) => return tracing::info!("keymap from XKB_DEFAULT_* environment"),
                Err(err) => tracing::warn!(?err, "XKB_DEFAULT_* keymap rejected"),
            }
        }

        if let Some(conf) = std::fs::read_to_string("/etc/X11/xorg.conf.d/00-keyboard.conf")
            .ok()
            .and_then(|text| parse_xorg_keyboard(&text))
        {
            let config = XkbConfig {
                model: &conf.model,
                layout: &conf.layout,
                variant: &conf.variant,
                options: (!conf.options.is_empty()).then(|| conf.options.clone()),
                ..Default::default()
            };
            match keyboard.set_xkb_config(self, config) {
                Ok(()) => {
                    return tracing::info!(
                        ?conf,
                        "keymap from /etc/X11/xorg.conf.d/00-keyboard.conf"
                    );
                }
                Err(err) => tracing::warn!(?conf, ?err, "system keyboard config rejected"),
            }
        }

        let fallback = XkbConfig {
            layout: "us",
            ..Default::default()
        };
        match keyboard.set_xkb_config(self, fallback) {
            Ok(()) => tracing::info!("keymap: fallback us"),
            Err(err) => tracing::error!(?err, "even the us fallback keymap failed to compile"),
        }
    }

    /// Applies `[input.keyboard]` after a reload: the keymap only when an XKB field changed
    /// (a swap resends the keymap to every client), the repeat info only when it changed.
    pub fn reapply_keyboard_config(&mut self, old: &crate::config::Input) {
        let new = self.config.input.keyboard.clone();
        if old.keyboard.xkb() != new.xkb() {
            self.apply_keymap();
        }
        let (rate, delay) = (new.repeat_rate, new.repeat_delay);
        if (old.keyboard.repeat_rate, old.keyboard.repeat_delay) != (rate, delay) {
            self.keyboard.change_repeat_info(rate, delay);
            tracing::info!(rate, delay, "keyboard repeat");
        }
    }
}

fn keymap_file_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AURORA_XKB_FILE") {
        return Some(path.into());
    }
    Some(crate::config::config_dir()?.join("keymap.xkb"))
}

fn parse_xorg_keyboard(text: &str) -> Option<XorgKeyboard> {
    let mut conf = XorgKeyboard::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("Option") {
            continue;
        }
        let (Some(key), Some(value)) = (words.next(), words.next()) else {
            continue;
        };
        let value = value.trim_matches('"').to_string();
        match key.trim_matches('"') {
            "XkbModel" => conf.model = value,
            "XkbLayout" => conf.layout = value,
            "XkbVariant" => conf.variant = value,
            "XkbOptions" => conf.options = value,
            _ => {}
        }
    }
    (!conf.layout.is_empty()).then_some(conf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_localed_snippet() {
        let text = "Section \"InputClass\"\n  Option \"XkbLayout\" \"us\"\n  Option \"XkbModel\" \"pc105\"\n  Option \"XkbVariant\" \"altgr-intl\"\nEndSection\n";
        let conf = parse_xorg_keyboard(text).unwrap();
        assert_eq!(
            (conf.layout.as_str(), conf.variant.as_str()),
            ("us", "altgr-intl")
        );
        assert_eq!(parse_xorg_keyboard("Section \"x\"\nEndSection"), None);
    }
}
