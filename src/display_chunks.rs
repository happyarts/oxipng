/// List of chunks that affect image display and will be kept when using the `Safe` chunk strip option
pub const DISPLAY_CHUNKS: [[u8; 4]; 9] = [
    *b"cICP", *b"iCCP", *b"sRGB", *b"gAMA", *b"cHRM", *b"pHYs", *b"acTL", *b"fcTL", *b"fdAT",
];
