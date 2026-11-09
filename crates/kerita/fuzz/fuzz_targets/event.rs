#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = kerita::fuzz::event(data);
    assert!(kerita::fuzz::round_trips(data), "a read event writes other bytes");
});
