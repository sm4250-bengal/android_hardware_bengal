//! Cargo integration check for the actual generated registry shipped by Android.
use bengal_hud::can::selection::VehicleSelection;
use std::{fs, path::Path, time::{SystemTime, UNIX_EPOCH}};

#[test]
fn shipped_catalogue_loads_every_available_profile_and_preserves_selection() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("bengal-shipped-catalogue-{stamp}"));
    fs::create_dir(&dir).unwrap();
    let selected = dir.join("selected");
    fs::write(&selected, "7021017").unwrap();
    let catalogue = Path::new(env!("CARGO_MANIFEST_DIR")).join("can/vehicles.tsv");
    let catalogue_text = fs::read_to_string(&catalogue).unwrap();
    let rows: Vec<Vec<&str>> = catalogue_text.lines().skip(1)
        .map(|line| line.split('\t').collect()).collect();
    assert_eq!(rows.len(), 3988);
    assert!(rows.iter().all(|row| row.len() == 8));
    let saved_row = rows.iter().find(|row| row[0] == "7021017").unwrap();
    let mut state = VehicleSelection::load(&catalogue, &selected);
    let page = state.catalogue_json(0, 32).unwrap();
    assert!(page.contains("\"total\":3988"), "{page}");
    assert!(state.state_json().contains("\"id\":\"7021017\""));
    assert_eq!(state.active_profile(), if saved_row[6] == "available" {
        Some(saved_row[7])
    } else { None });
    assert_eq!(fs::read_to_string(&selected).unwrap(), "7021017");
    let available: Vec<_> = rows.iter().filter(|row| row[6] == "available").collect();
    assert!(available.len() >= 2);
    for row in available {
        state.select(row[0]).unwrap();
        assert_eq!(state.active_profile(), Some(row[7]), "selection {}", row[0]);
        let reloaded = VehicleSelection::load(&catalogue, &selected);
        assert_eq!(reloaded.active_profile(), Some(row[7]), "reload {}", row[0]);
    }
    for (id, profile) in [("2009002", "xinpu-vw-pq"), ("1001001", "raise-vw-pq")] {
        let row = rows.iter().find(|row| row[0] == id).unwrap();
        assert_eq!(row[6], "available");
        assert_eq!(row[7], profile);
    }
    fs::remove_dir_all(dir).unwrap();
}
