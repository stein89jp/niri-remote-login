/// RDP Unicode keyboard messages carry UTF-16 code units. Release messages
/// may appear between the surrogate halves, so only presses advance decoding.
#[derive(Default)]
pub(crate) struct UnicodeInput {
    high_surrogate: Option<u16>,
}
impl UnicodeInput {
    pub(crate) fn press(&mut self, unit: u16) -> Option<char> {
        let pending = self.high_surrogate.take();
        match unit {
            0xd800..=0xdbff => {
                self.high_surrogate = Some(unit);
                None
            }
            0xdc00..=0xdfff => pending.and_then(|high| {
                char::from_u32(
                    0x10000 + ((u32::from(high) - 0xd800) << 10) + u32::from(unit) - 0xdc00,
                )
            }),
            _ => char::from_u32(u32::from(unit)).filter(|c| *c != '\0'),
        }
    }
}
