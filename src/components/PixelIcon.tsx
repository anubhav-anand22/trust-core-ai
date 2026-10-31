// 8×8 pixel-art icons, drawn as SVG squares from a bitmap string.
//
// Why not an icon font or an icon package: the app runs offline, so anything
// fetched at runtime is out, and a whole icon library for seven glyphs is
// weight we'd ship for nothing. A bitmap is also the pixel look itself — the
// same 8×8 grid as the logo — rather than a smooth icon pretending to be one.
// `currentColor` lets each icon take the colour of the text around it.

const ICONS = {
  doc: [
    "1111100.",
    "1...1110",
    "1...1111",
    "1......1",
    "1.1111.1",
    "1......1",
    "1.1111.1",
    "11111111",
  ],
  image: [
    "11111111",
    "1......1",
    "1....1.1",
    "1......1",
    "1..1...1",
    "1.111.11",
    "1111111.",
    "11111111",
  ],
  audio: [
    "...11...",
    "..1111..",
    "..1111..",
    "..1111..",
    "1.1111.1",
    "1..11..1",
    ".111111.",
    "...11...",
  ],
  spark: [
    "...1....",
    "...1....",
    "..111...",
    "1111111.",
    "..111...",
    "...1....",
    "...1....",
    "........",
  ],
  lock: [
    "..1111..",
    ".1....1.",
    ".1....1.",
    "11111111",
    "111..111",
    "111..111",
    "11111111",
    "11111111",
  ],
  clip: [
    "....111.",
    "...1...1",
    "..1..1.1",
    ".1..1.1.",
    "1..1.1..",
    "1.1.1...",
    "1...1...",
    ".111....",
  ],
  run: [
    "11......",
    "1111....",
    "1..111..",
    "1....111",
    "1..111..",
    "1111....",
    "11......",
    "........",
  ],
} as const;

export type PixelIconName = keyof typeof ICONS;

export function PixelIcon({ name, size = 16 }: { name: PixelIconName; size?: number }) {
  const rows = ICONS[name];
  return (
    <svg
      className="pixel-icon"
      width={size}
      height={size}
      viewBox="0 0 8 8"
      shapeRendering="crispEdges"
      aria-hidden
    >
      {rows.flatMap((row, y) =>
        [...row].map((c, x) =>
          c === "1" ? <rect key={`${x}-${y}`} x={x} y={y} width={1} height={1} fill="currentColor" /> : null,
        ),
      )}
    </svg>
  );
}
