//! The decoder must reject any malformed bitstream without panicking.
#![no_main]

use fernsicht_codec::synthetic::SyntheticDecoder;
use fernsicht_codec::{DecodedFrame, Decoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = DecodedFrame::default();
    let _ = SyntheticDecoder::default().decode(data, &mut d);
});
