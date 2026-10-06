use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneSnapshot {
    pub power: String,
    pub input: String,
    pub volume: i32,
}

#[derive(Debug, Clone)]
pub struct ZoneSelection {
    pub main: bool,
    pub zone2: bool,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Action {
    SetPower { zone: String, on: bool },
    SetInput { zone: String, input: String },
    SetVolume { zone: String, raw: i32 },
}

/// Compute the actions needed to apply the zone policy
pub fn plan(
    snapshots: &HashMap<String, ZoneSnapshot>,
    selected: &ZoneSelection,
    current: &HashMap<String, ZoneSnapshot>,
    start_volumes: &HashMap<String, i32>,
) -> Vec<Action> {
    let mut actions = Vec::new();

    for (zone_name, selected_on) in &[("main", selected.main), ("zone2", selected.zone2)] {
        let zone = zone_name.to_string();
        let snapshot = snapshots.get(*zone_name);
        let current_state = current.get(*zone_name);

        if let (Some(snap), Some(curr)) = (snapshot, current_state) {
            if *selected_on {
                // Zone is selected: ensure it's on and on airplay
                if curr.power != "on" {
                    actions.push(Action::SetPower { zone: zone.clone(), on: true });
                }
                if curr.input != "airplay" {
                    actions.push(Action::SetInput {
                        zone: zone.clone(),
                        input: "airplay".to_string(),
                    });
                }
                // Set default start volume only if snapshot was standby
                if snap.power == "standby" {
                    if let Some(&start_vol) = start_volumes.get(*zone_name) {
                        actions.push(Action::SetVolume {
                            zone: zone.clone(),
                            raw: start_vol,
                        });
                    }
                }
            } else if snap.power == "standby" || snap.input == "airplay" {
                // Not selected, and it was either idle or only ever on our own
                // airplay input (an explicit deselect): put it in standby.
                if curr.power != "standby" {
                    actions.push(Action::SetPower { zone: zone.clone(), on: false });
                }
            } else {
                // Not selected and someone else is using it (TV on hdmi1, or another
                // network source such as spotify/net_radio): restore its previous
                // input and volume and keep it on.
                if curr.power != "on" {
                    actions.push(Action::SetPower { zone: zone.clone(), on: true });
                }
                if curr.input != snap.input {
                    actions.push(Action::SetInput {
                        zone: zone.clone(),
                        input: snap.input.clone(),
                    });
                }
                // Always restore volume to ensure exact previous state
                actions.push(Action::SetVolume {
                    zone: zone.clone(),
                    raw: snap.volume,
                });
            }
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(power: &str, input: &str, volume: i32) -> ZoneSnapshot {
        ZoneSnapshot {
            power: power.to_string(),
            input: input.to_string(),
            volume,
        }
    }

    #[test]
    fn test_both_zones_selected_from_standby() {
        let mut snapshots = HashMap::new();
        snapshots.insert("main".to_string(), snapshot("standby", "airplay", 95));
        snapshots.insert("zone2".to_string(), snapshot("standby", "airplay", 151));

        let mut current = HashMap::new();
        current.insert("main".to_string(), snapshot("on", "airplay", 95));
        current.insert("zone2".to_string(), snapshot("on", "airplay", 151));

        let selection = ZoneSelection { main: true, zone2: true };

        let mut start_volumes = HashMap::new();
        start_volumes.insert("main".to_string(), 95);
        start_volumes.insert("zone2".to_string(), 151);

        let actions = plan(&snapshots, &selection, &current, &start_volumes);

        // Both zones already on+airplay, but need volume set since they were in standby
        assert!(actions.contains(&Action::SetVolume { zone: "main".to_string(), raw: 95 }));
        assert!(actions.contains(&Action::SetVolume { zone: "zone2".to_string(), raw: 151 }));
    }

    #[test]
    fn test_tv_on_hdmi1_upstairs_selected() {
        // main is watching TV (hdmi1), upstairs is selected for radio
        let mut snapshots = HashMap::new();
        snapshots.insert("main".to_string(), snapshot("on", "hdmi1", 100));
        snapshots.insert("zone2".to_string(), snapshot("standby", "airplay", 151));

        let mut current = HashMap::new();
        current.insert("main".to_string(), snapshot("on", "airplay", 100)); // AirPlay grabbed it
        current.insert("zone2".to_string(), snapshot("on", "airplay", 151));

        let selection = ZoneSelection { main: false, zone2: true };

        let mut start_volumes = HashMap::new();
        start_volumes.insert("zone2".to_string(), 151);

        let actions = plan(&snapshots, &selection, &current, &start_volumes);

        // main should be restored to hdmi1 and kept on
        assert!(actions.contains(&Action::SetInput {
            zone: "main".to_string(),
            input: "hdmi1".to_string()
        }));
        assert!(actions.contains(&Action::SetVolume { zone: "main".to_string(), raw: 100 }));

        // zone2 should get start volume since it was in standby
        assert!(actions.contains(&Action::SetVolume { zone: "zone2".to_string(), raw: 151 }));

        // main should NOT be powered off
        assert!(!actions.contains(&Action::SetPower { zone: "main".to_string(), on: false }));
    }

    #[test]
    fn test_no_zone_selected() {
        let mut snapshots = HashMap::new();
        snapshots.insert("main".to_string(), snapshot("standby", "airplay", 95));
        snapshots.insert("zone2".to_string(), snapshot("standby", "airplay", 151));

        let mut current = HashMap::new();
        current.insert("main".to_string(), snapshot("on", "airplay", 95));
        current.insert("zone2".to_string(), snapshot("on", "airplay", 151));

        let selection = ZoneSelection { main: false, zone2: false };

        let start_volumes = HashMap::new();

        let actions = plan(&snapshots, &selection, &current, &start_volumes);

        // Both zones should be put back to standby
        assert!(actions.contains(&Action::SetPower { zone: "main".to_string(), on: false }));
        assert!(actions.contains(&Action::SetPower { zone: "zone2".to_string(), on: false }));
    }

    #[test]
    fn test_already_playing_no_volume_bump() {
        // Both zones already on airplay (already playing)
        let mut snapshots = HashMap::new();
        snapshots.insert("main".to_string(), snapshot("on", "airplay", 95));
        snapshots.insert("zone2".to_string(), snapshot("on", "airplay", 151));

        let mut current = HashMap::new();
        current.insert("main".to_string(), snapshot("on", "airplay", 95));
        current.insert("zone2".to_string(), snapshot("on", "airplay", 151));

        let selection = ZoneSelection { main: true, zone2: true };

        let mut start_volumes = HashMap::new();
        start_volumes.insert("main".to_string(), 95);
        start_volumes.insert("zone2".to_string(), 151);

        let actions = plan(&snapshots, &selection, &current, &start_volumes);

        // No volume changes since they weren't in standby
        assert!(!actions.iter().any(|a| matches!(a, Action::SetVolume { .. })));
    }

    fn single_zone(snap: ZoneSnapshot, curr: ZoneSnapshot) -> (HashMap<String, ZoneSnapshot>, HashMap<String, ZoneSnapshot>) {
        let mut snapshots = HashMap::new();
        snapshots.insert("main".to_string(), snap);
        let mut current = HashMap::new();
        current.insert("main".to_string(), curr);
        (snapshots, current)
    }

    #[test]
    fn test_deselected_airplay_zone_goes_to_standby() {
        let (snapshots, current) = single_zone(
            snapshot("on", "airplay", 95),
            snapshot("on", "airplay", 95),
        );
        let selection = ZoneSelection { main: false, zone2: false };
        let actions = plan(&snapshots, &selection, &current, &HashMap::new());
        assert_eq!(actions, vec![Action::SetPower { zone: "main".to_string(), on: false }]);
    }

    #[test]
    fn test_deselected_other_network_input_is_restored_not_powered_off() {
        let (snapshots, current) = single_zone(
            snapshot("on", "spotify", 88),
            snapshot("on", "airplay", 95), // AirPlay grabbed it
        );
        let selection = ZoneSelection { main: false, zone2: false };
        let actions = plan(&snapshots, &selection, &current, &HashMap::new());
        assert_eq!(
            actions,
            vec![
                Action::SetInput { zone: "main".to_string(), input: "spotify".to_string() },
                Action::SetVolume { zone: "main".to_string(), raw: 88 },
            ]
        );
    }
}
