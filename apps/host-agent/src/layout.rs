//! Where the captured monitor lies in the desktop, for mapping absolute
//! pointer input (compositors spread an absolute pointer over all
//! monitors together).
//!
//! KMS knows the monitors but not how the compositor arranges them. KDE
//! Plasma stores the arrangement in `~/.config/kwinoutputconfig.json`; it
//! is read from the home of the desktop's user (see
//! `fernsicht_core::desktop`). Elsewhere, or if that fails, the captured
//! monitor is taken as the whole desktop (right for a single monitor).

use std::collections::BTreeSet;
use std::path::PathBuf;

use fernsicht_input::uinput::AbsArea;
use serde::Deserialize;

#[derive(Deserialize)]
struct Section {
    name: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    connector_name: String,
    mode: Mode,
    #[serde(default = "one")]
    scale: f64,
    #[serde(default)]
    transform: String,
}

fn one() -> f64 {
    1.0
}

#[derive(Deserialize)]
struct Mode {
    width: f64,
    height: f64,
}

#[derive(Deserialize)]
struct Setup {
    outputs: Vec<SetupOutput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetupOutput {
    enabled: bool,
    output_index: usize,
    position: Position,
}

#[derive(Deserialize)]
struct Position {
    x: f64,
    y: f64,
}

/// A monitor's rectangle in desktop coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// The captured monitor's area, from KWin's output configuration:
/// `captured` is its connector, `active` the connectors that are on.
pub fn from_kwin(json: &str, captured: &str, active: &[String]) -> Result<AbsArea, String> {
    let sections: Vec<Section> =
        serde_json::from_str(json).map_err(|e| format!("not KWin's output config: {e}"))?;
    let data = |name: &str| {
        sections
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.data.clone())
            .ok_or_else(|| format!("no \"{name}\" in KWin's output config"))
    };
    let outputs: Vec<Output> =
        serde_json::from_value(data("outputs")?).map_err(|e| format!("outputs: {e}"))?;
    let setups: Vec<Setup> =
        serde_json::from_value(data("setups")?).map_err(|e| format!("setups: {e}"))?;

    let wanted: BTreeSet<&str> = active.iter().map(String::as_str).collect();
    // The arrangement saved for exactly the monitors that are on now.
    let setup = setups
        .iter()
        .find(|s| {
            let names: Option<BTreeSet<&str>> = s
                .outputs
                .iter()
                .filter(|o| o.enabled)
                .map(|o| {
                    outputs
                        .get(o.output_index)
                        .map(|out| out.connector_name.as_str())
                })
                .collect();
            names.is_some_and(|n| n == wanted)
        })
        .ok_or_else(|| format!("no saved arrangement for {active:?}"))?;

    let mut rects = Vec::new();
    let mut target = None;
    for o in setup.outputs.iter().filter(|o| o.enabled) {
        let out = &outputs[o.output_index];
        let scale = if out.scale > 0.0 { out.scale } else { 1.0 };
        let (mut w, mut h) = (out.mode.width / scale, out.mode.height / scale);
        if out.transform.contains("90") || out.transform.contains("270") {
            std::mem::swap(&mut w, &mut h);
        }
        let r = Rect {
            x: o.position.x,
            y: o.position.y,
            w,
            h,
        };
        if out.connector_name == captured {
            target = Some(r);
        }
        rects.push(r);
    }
    let t = target.ok_or_else(|| format!("{captured} is not in the arrangement"))?;
    let min_x = rects.iter().map(|r| r.x).fold(f64::INFINITY, f64::min);
    let min_y = rects.iter().map(|r| r.y).fold(f64::INFINITY, f64::min);
    let max_x = rects
        .iter()
        .map(|r| r.x + r.w)
        .fold(f64::NEG_INFINITY, f64::max);
    let max_y = rects
        .iter()
        .map(|r| r.y + r.h)
        .fold(f64::NEG_INFINITY, f64::max);
    let (dw, dh) = (max_x - min_x, max_y - min_y);
    if dw <= 0.0 || dh <= 0.0 {
        return Err("empty desktop".into());
    }
    Ok(AbsArea {
        x: (t.x - min_x) / dw,
        y: (t.y - min_y) / dh,
        width: t.w / dw,
        height: t.h / dh,
    })
}

/// KWin's output config of the desktop's user (also when the host runs as
/// root: sudo, or a system service).
fn kwin_config_path() -> Option<PathBuf> {
    let user = fernsicht_core::desktop::desktop_user()?;
    Some(user.home.join(".config/kwinoutputconfig.json"))
}

/// The captured monitor's area in the desktop, or the whole desktop when
/// the arrangement is unknown (logged).
pub fn input_area(captured: &str, active: &[String]) -> AbsArea {
    if active.len() <= 1 {
        return AbsArea::default();
    }
    let found = kwin_config_path()
        .ok_or_else(|| "no home directory".to_string())
        .and_then(|p| std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display())))
        .and_then(|json| from_kwin(&json, captured, active));
    match found {
        Ok(area) => {
            log::info!("pointer input mapped to {captured}: {area:?} of the desktop");
            area
        }
        Err(e) => {
            log::warn!(
                "monitor arrangement unknown ({e}); the pointer may land on another monitor"
            );
            AbsArea::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structure of zentrale's file: DP-2 left, DP-1 right, plus
    /// remembered arrangements with other monitors.
    const ZENTRALE: &str = r#"[
      {"name": "outputs", "data": [
        {"connectorName": "DP-1", "edidIdentifier": "AOC 1", "mode": {"width": 2560, "height": 1440, "refreshRate": 165000, "flags": 0}, "scale": 1, "transform": "Normal"},
        {"connectorName": "DP-2", "edidIdentifier": "AOC 2", "mode": {"width": 2560, "height": 1440, "refreshRate": 165000, "flags": 0}, "scale": 1, "transform": "Normal"},
        {"connectorName": "HDMI-A-1", "edidIdentifier": "LHC", "mode": {"width": 3440, "height": 1440, "refreshRate": 59999, "flags": 1}, "scale": 1, "transform": "Normal"}
      ]},
      {"name": "setups", "data": [
        {"lidClosed": false, "outputs": [
          {"enabled": true, "outputIndex": 0, "position": {"x": 2560, "y": 0}, "priority": 2, "replicationSource": ""},
          {"enabled": true, "outputIndex": 1, "position": {"x": 0, "y": 0}, "priority": 1, "replicationSource": ""}]},
        {"lidClosed": false, "outputs": [
          {"enabled": true, "outputIndex": 1, "position": {"x": 0, "y": 0}, "priority": 0, "replicationSource": ""},
          {"enabled": true, "outputIndex": 0, "position": {"x": 2560, "y": 0}, "priority": 1, "replicationSource": ""},
          {"enabled": true, "outputIndex": 2, "position": {"x": 5120, "y": 0}, "priority": 2, "replicationSource": ""}]}
      ]}
    ]"#;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn two_monitors_side_by_side() {
        let active = names(&["DP-1", "DP-2"]);
        let right = from_kwin(ZENTRALE, "DP-1", &active).unwrap();
        assert_eq!(
            right,
            AbsArea {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 1.0
            }
        );
        let left = from_kwin(ZENTRALE, "DP-2", &active).unwrap();
        assert_eq!((left.x, left.width), (0.0, 0.5));
    }

    #[test]
    fn the_arrangement_for_the_monitors_that_are_on_is_used() {
        // With the ultrawide on: 2560 + 2560 + 3440 = 8560 wide.
        let active = names(&["HDMI-A-1", "DP-1", "DP-2"]);
        let a = from_kwin(ZENTRALE, "HDMI-A-1", &active).unwrap();
        assert!((a.x - 5120.0 / 8560.0).abs() < 1e-9);
        assert!((a.width - 3440.0 / 8560.0).abs() < 1e-9);
    }

    #[test]
    fn scale_and_rotation_change_the_size() {
        let json = r#"[
          {"name": "outputs", "data": [
            {"connectorName": "eDP-1", "mode": {"width": 2880, "height": 1800}, "scale": 2, "transform": "Normal"},
            {"connectorName": "DP-1", "mode": {"width": 1920, "height": 1080}, "scale": 1, "transform": "Rotated90"}]},
          {"name": "setups", "data": [
            {"outputs": [
              {"enabled": true, "outputIndex": 0, "position": {"x": 0, "y": 0}},
              {"enabled": true, "outputIndex": 1, "position": {"x": 1440, "y": 0}}]}]}
        ]"#;
        // eDP-1: 1440×900 logical; DP-1 portrait 1080×1920. Desktop 2520×1920.
        let active = names(&["eDP-1", "DP-1"]);
        let laptop = from_kwin(json, "eDP-1", &active).unwrap();
        assert!((laptop.width - 1440.0 / 2520.0).abs() < 1e-9);
        assert!((laptop.height - 900.0 / 1920.0).abs() < 1e-9);
        let portrait = from_kwin(json, "DP-1", &active).unwrap();
        assert!((portrait.height - 1.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_situations_are_errors() {
        let active = names(&["DP-1", "DP-2"]);
        assert!(from_kwin("{}", "DP-1", &active).is_err());
        assert!(from_kwin("[]", "DP-1", &active).is_err());
        assert!(from_kwin(ZENTRALE, "DP-9", &active).is_err());
        assert!(from_kwin(ZENTRALE, "DP-1", &names(&["DP-1", "HDMI-A-1"])).is_err());
    }

    #[test]
    fn a_single_monitor_needs_no_config() {
        assert_eq!(input_area("DP-1", &names(&["DP-1"])), AbsArea::default());
    }
}
