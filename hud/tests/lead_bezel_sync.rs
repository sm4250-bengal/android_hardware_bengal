use bengal_hud::bezel::{BezelDecoder, BezelPress};

const ABS: u16 = 3;
const KEY: u16 = 1;
const SLOT: u16 = 0x2f;
const ID: u16 = 0x39;
const X: u16 = 0x35;
const Y: u16 = 0x36;
const BTN_TOUCH: u16 = 0x14a;

fn frame(decoder: &mut BezelDecoder) -> Option<BezelPress> {
    decoder.ingest(0, 0, 0)
}

fn start(decoder: &mut BezelDecoder, slot: i32, id: i32) {
    decoder.ingest(ABS, SLOT, slot);
    decoder.ingest(ABS, ID, id);
}

fn release(decoder: &mut BezelDecoder) -> Option<BezelPress> {
    decoder.ingest(ABS, ID, -1);
    frame(decoder)
}

#[test]
fn repeated_tap_with_unchanged_axes_is_not_lost() {
    let mut decoder = BezelDecoder::new();
    start(&mut decoder, 0, 10);
    decoder.ingest(ABS, X, -30);
    decoder.ingest(ABS, Y, 189);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), Some(BezelPress { x: -30, y: 189 }));

    // Type-B suppresses unchanged slot axes across tracking-ID lifetimes.
    start(&mut decoder, 0, 11);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), Some(BezelPress { x: -30, y: 189 }));
}

#[test]
fn a_new_lifetime_uses_one_new_axis_and_one_cached_axis_at_its_first_frame() {
    let mut decoder = BezelDecoder::new();
    start(&mut decoder, 0, 10);
    decoder.ingest(ABS, X, -30);
    decoder.ingest(ABS, Y, 189);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), Some(BezelPress { x: -30, y: 189 }));

    start(&mut decoder, 0, 11);
    decoder.ingest(ABS, Y, 264);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), Some(BezelPress { x: -30, y: 264 }));

    start(&mut decoder, 0, 12);
    decoder.ingest(ABS, X, -35);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), Some(BezelPress { x: -35, y: 264 }));
}

#[test]
fn dropped_unseen_screen_finger_requires_explicit_all_up_before_rearming() {
    let mut decoder = BezelDecoder::new();
    decoder.ingest(ABS, SLOT, 0);
    decoder.ingest(0, 3, 0); // SYN_DROPPED
    frame(&mut decoder); // fence, not proof that every finger is up

    start(&mut decoder, 1, 25);
    decoder.ingest(ABS, X, -30);
    decoder.ingest(ABS, Y, 189);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), None);

    decoder.ingest(KEY, BTN_TOUCH, 0);
    assert_eq!(frame(&mut decoder), None);
    start(&mut decoder, 0, 26);
    decoder.ingest(ABS, X, -30);
    decoder.ingest(ABS, Y, 189);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), Some(BezelPress { x: -30, y: 189 }));
}

#[test]
fn partial_post_drop_frames_are_ignored_before_the_explicit_all_up() {
    let mut decoder = BezelDecoder::new();
    decoder.ingest(0, 3, 0); // SYN_DROPPED
    frame(&mut decoder);
    start(&mut decoder, 0, 10);
    decoder.ingest(ABS, X, -30);
    frame(&mut decoder);
    decoder.ingest(ABS, Y, 189);
    assert_eq!(release(&mut decoder), None);
}

#[test]
fn a_drop_invalidates_cached_axes_until_a_new_contact_reports_them() {
    let mut decoder = BezelDecoder::new();
    start(&mut decoder, 0, 10);
    decoder.ingest(ABS, X, -30);
    decoder.ingest(ABS, Y, 189);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), Some(BezelPress { x: -30, y: 189 }));

    decoder.ingest(0, 3, 0); // SYN_DROPPED
    frame(&mut decoder);
    decoder.ingest(KEY, BTN_TOUCH, 0);
    frame(&mut decoder);

    start(&mut decoder, 0, 11);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), None, "pre-drop axes are untrusted");
    start(&mut decoder, 0, 12);
    decoder.ingest(ABS, X, -30);
    decoder.ingest(ABS, Y, 189);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), Some(BezelPress { x: -30, y: 189 }));
}

#[test]
fn another_drop_invalidates_a_pending_all_up_witness() {
    let mut decoder = BezelDecoder::new();
    decoder.ingest(0, 3, 0); // SYN_DROPPED
    frame(&mut decoder);
    decoder.ingest(KEY, BTN_TOUCH, 0);
    decoder.ingest(0, 3, 0); // a later unseen screen contact resets trust
    frame(&mut decoder);
    start(&mut decoder, 1, 1);
    decoder.ingest(ABS, X, -30);
    decoder.ingest(ABS, Y, 189);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), None);
}

#[test]
fn changing_contact_slot_without_an_all_up_frame_is_not_a_tap() {
    let mut decoder = BezelDecoder::new();
    start(&mut decoder, 0, 10);
    decoder.ingest(ABS, X, -30);
    decoder.ingest(ABS, Y, 189);
    frame(&mut decoder);
    decoder.ingest(ABS, ID, -1);
    start(&mut decoder, 1, 11);
    decoder.ingest(ABS, X, -30);
    decoder.ingest(ABS, Y, 346);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), None);
}

#[test]
fn first_frame_can_replace_a_cached_on_panel_x_before_its_y_arrives() {
    let mut decoder = BezelDecoder::new();
    start(&mut decoder, 0, 10);
    decoder.ingest(ABS, X, 40);
    decoder.ingest(ABS, Y, 189);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), None);

    start(&mut decoder, 0, 11);
    decoder.ingest(ABS, Y, 264);
    decoder.ingest(ABS, X, -30);
    frame(&mut decoder);
    assert_eq!(release(&mut decoder), Some(BezelPress { x: -30, y: 264 }));
}
