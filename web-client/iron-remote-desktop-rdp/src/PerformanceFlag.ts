/**
 * Performance flags per [MS-RDPBCGR] 2.2.1.11.1.1.1 (`TS_EXTENDED_INFO_PACKET.performanceFlags`).
 *
 * Combine them with `|` and pass the result to `performanceFlags()`.
 */
export const PerformanceFlag = {
    DISABLE_WALLPAPER: 0x1,
    DISABLE_FULLWINDOWDRAG: 0x2,
    DISABLE_MENUANIMATIONS: 0x4,
    DISABLE_THEMING: 0x8,
    DISABLE_CURSOR_SHADOW: 0x20,
    DISABLE_CURSORSETTINGS: 0x40,
    ENABLE_FONT_SMOOTHING: 0x80,
    ENABLE_DESKTOP_COMPOSITION: 0x100,
} as const;

/** Flags sent when `performanceFlags()` is not used. */
export const DEFAULT_PERFORMANCE_FLAGS: number =
    PerformanceFlag.DISABLE_FULLWINDOWDRAG |
    PerformanceFlag.DISABLE_MENUANIMATIONS |
    PerformanceFlag.ENABLE_FONT_SMOOTHING;
