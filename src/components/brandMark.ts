/**
 * The Mewrk brand mark, as path data.
 *
 * The mark is a text cursor with a cat's ears: one monospace cell whose two top corners rise
 * into ears. It reads as the cursor first and the cat second, and its top edge is the upper
 * half of an M. Five rules draw it, and every size is the same drawing because of that:
 *
 * - the box is 120 × 200, the 3:5 of a monospace cell;
 * - the ear roots sit at a quarter of the height, y=50;
 * - both inner ear lines run at 45°;
 * - the flat between the ears is a sixth of the width, x=50–70;
 * - the bottom corners are rounded to a twelfth of the width, and the ear tips to match.
 *
 * Every shipped brand asset is cut from these strings — `src/mewrk-mark.svg`,
 * `src/logo.svg`, `src/mewrk-icon.svg` and `src/mewrk-icon-small.svg` — and
 * `MewrkIcon.tsx` renders them in the app. `MewrkIcon.test.tsx` pins the files to the
 * strings, so the shipped assets and the in-app copies cannot drift.
 *
 * The cat on the composer rim and the one walking beside a streaming round are a different
 * drawing — the mascot, in `catArt.ts` — and never share a surface with this mark.
 */
export const MARK_PATH =
  "M0 190V12Q0 0 8.5 8.5L45.8 45.8Q50 50 56 50H64Q70 50 74.2 45.8L111.5 8.5Q120 0 120 12V190A10 10 0 0 1 110 200H10A10 10 0 0 1 0 190Z";

/** The mark's own box. Width over height is the monospace cell's 3:5. */
export const MARK_VIEWBOX = "0 0 120 200";

/**
 * "Mewrk", outlined from Archivo SemiBold (SIL Open Font License 1.1) at −2.5% tracking, so
 * the wordmark renders the same with no font installed.
 *
 * Drawn to the mark's scale: the cap height is 200, the mark's height, with the baseline at
 * y=200 and the M's ink starting at x=0. The k's ascender reaches y=−10.8 and the round
 * letters overshoot the baseline to y=203.5, which is what {@link LOCKUP_VIEWBOX} leaves room
 * for.
 */
export const WORDMARK_PATH =
  "M36.4 200L0 200L0 0L59.8 0L91.3 111.4Q93 117.2 94.8 124.5Q96.5 131.8 98.1 138.5Q99.7 145.2 100.6 150.1L102.9 150.1Q103.5 145.8 104.7 139.4Q105.8 132.9 107.4 125.5Q109 118.1 111.1 111.1L142.6 0L201.7 0L201.7 200L163.8 200L163.8 98.8Q163.8 85.4 164.1 72Q164.4 58.6 164.9 49Q165.3 39.4 165.3 37.6L163 37.6Q162.4 40.2 160.1 49Q157.7 57.7 155.1 67.9Q152.5 78.1 150.4 85.4L117.5 200L82.5 200L49.6 85.7Q47.8 79.6 45.6 71Q43.4 62.4 41.3 53.4Q39.1 44.3 37.3 37.6L35 37.6Q35.3 45.5 35.6 56.4Q35.9 67.3 36.2 78.6Q36.4 89.8 36.4 98.8L36.4 200ZM301.5 203.5Q277 203.5 260.6 194.9Q244.3 186.3 236.2 168.5Q228 150.7 228 123.3Q228 95.6 236.2 78Q244.3 60.3 260.6 51.7Q277 43.1 301.5 43.1Q323.6 43.1 338.6 51.5Q353.6 59.8 361.2 77Q368.8 94.2 368.8 121.6L368.8 132.1L264.4 132.1Q265 146.4 268.8 156.1Q272.6 165.9 280.6 170.7Q288.6 175.5 301.7 175.5Q308.5 175.5 314.3 173.8Q320.1 172 324.5 168.4Q328.9 164.7 331.3 159.2Q333.8 153.6 333.8 146.4L368.8 146.4Q368.8 160.9 363.7 171.7Q358.6 182.5 349.4 189.5Q340.2 196.5 328 200Q315.7 203.5 301.5 203.5ZM265 107L331.8 107Q331.8 97.4 329.6 90.7Q327.4 84 323.5 79.6Q319.5 75.2 314 73.3Q308.5 71.4 301.5 71.4Q290.1 71.4 282.4 75.2Q274.6 79 270.6 86.9Q266.5 94.8 265 107ZM457.1 200L419.5 200L373.8 46.6L410.5 46.6L431.8 130Q433.5 136.4 434.7 142.7Q435.9 149 436.7 153.4Q437.6 157.7 437.9 158.6L439.7 158.6Q440.8 153.4 442 147.2Q443.1 141.1 444.2 136Q445.2 130.9 445.5 129.2L465.3 46.6L503.5 46.6L524.2 129.4Q525.4 133.5 526.4 138.9Q527.4 144.3 528.6 149.6Q529.7 154.8 530.3 158.6L532.1 158.6Q532.7 155.1 533.7 150.1Q534.7 145.2 535.9 139.9Q537 134.7 537.9 130.3L558.9 46.6L593 46.6L547.5 200L509.9 200L492.4 126.8Q491.3 121.3 489.7 114.3Q488 107.3 486.7 100.3Q485.4 93.3 484.5 88.3L482.8 88.3Q482.8 90.4 481.9 95.6Q481 100.9 479.4 108.7Q477.8 116.6 475.2 126.8L457.1 200ZM641.1 200L605.5 200L605.5 46.6L635.3 46.6L638.2 70.8L640.2 70.8Q643.1 63.6 647.4 57.3Q651.6 51 658.3 47.1Q665 43.1 674.6 43.1Q679.3 43.1 683.2 44Q687.2 44.9 689.2 45.8L689.2 79.3L678.4 79.3Q669.4 79.3 662.4 81.8Q655.4 84.3 650.6 89.5Q645.8 94.8 643.4 102.6Q641.1 110.5 641.1 121L641.1 200ZM739.4 200L703.8 200L703.8-10.8L739.4-10.8L739.4 112L794.5 46.6L836.2 46.6L784.8 106.1L839.1 200L798.3 200L762.1 132.1L739.4 153.9L739.4 200Z";

/**
 * Where the mark stands in the lockup: one stem width (35.6, the l's) past the k's ink, on
 * the baseline, its ear tips on the cap line — the cursor resting after the last letter typed.
 */
export const LOCKUP_MARK_TRANSFORM = "translate(874.7 0)";

export const LOCKUP_VIEWBOX = "0 -12 995 216";

/**
 * The application icon, on a 100-unit plate: a prompt chevron and the mark after it, like a
 * terminal waiting for input. The shipped icon files inset this plate for the OS's margins;
 * the in-app copy draws it edge to edge.
 */
export const ICON_PROMPT_PATH = "M29.5 41.5 40.5 52 29.5 62.5";

/** The mark on the icon plate: 38 units tall, beside the prompt. */
export const ICON_MARK_TRANSFORM = "translate(50.5 31) scale(0.19)";

/**
 * The prompt and the mark without the plate: its middle 60 units, which holds both with room for
 * the prompt's round caps and keeps them centred as the plate does.
 */
export const ICON_BARE_VIEWBOX = "20 20 60 60";

/**
 * The mark alone on the plate, for frames of 24px and below, where the prompt's stroke would
 * be thinner than a pixel.
 */
export const ICON_SMALL_MARK_TRANSFORM = "translate(32.6 21) scale(0.29)";

/** Fixed brand colors. The icon is a picture of the app, so it does not follow the theme. */
export const BRAND_PLATE = "#1b1b1a";
export const BRAND_PROMPT = "#8c8a82";
export const BRAND_AMBER = "#c98a1b";
