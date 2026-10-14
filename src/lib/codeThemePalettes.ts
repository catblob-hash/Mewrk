import type { CodeTheme } from "./codeThemes";

/**
 * Published code themes, each reduced to the colours `CodeTheme` has a place for.
 *
 * The values are read from the themes' own VS Code definitions: the editor's ground, text
 * and line numbers, and the colour each definition gives the TextMate scope nearest to a
 * token kind (`entity.name.function` for a call, `support.type.property-name` for a key).
 * A colour a definition writes with alpha is flattened onto its ground. Where a definition
 * leaves a field to the editor's defaults, or names a diff hue only as a dimmed decoration,
 * the entry takes what the theme shows in the editor. The highlighter is lexical
 * (`codeHighlight.ts`), so a theme here is its palette, not every rule it has.
 */
export const PUBLISHED_CODE_THEMES: readonly CodeTheme[] = [
  {
    id: "github-light", name: "GitHub Light", scheme: "day",
    background: "#ffffff", foreground: "#1f2328", lineNumber: "#8c959f",
    comment: "#6e7781", commentItalic: false,
    string: "#0a3069", number: "#0550ae", keyword: "#cf222e", function: "#8250df", type: "#953800",
    meta: "#0550ae", tag: "#116329", attribute: "#0550ae", property: "#116329", variable: "#1f2328",
    added: "#1a7f37", removed: "#cf222e"
  },
  {
    id: "light-plus", name: "Light+", scheme: "day",
    background: "#ffffff", foreground: "#000000", lineNumber: "#237893",
    comment: "#008000", commentItalic: false,
    string: "#a31515", number: "#098658", keyword: "#0000ff", function: "#795e26", type: "#267f99",
    meta: "#af00db", tag: "#800000", attribute: "#e50000", property: "#0451a5", variable: "#001080",
    added: "#098658", removed: "#a31515"
  },
  {
    id: "one-light", name: "One Light", scheme: "day",
    background: "#fafafa", foreground: "#383a42", lineNumber: "#9d9d9f",
    comment: "#a0a1a7", commentItalic: true,
    string: "#50a14f", number: "#986801", keyword: "#a626a4", function: "#4078f2", type: "#c18401",
    meta: "#986801", tag: "#e45649", attribute: "#986801", property: "#e45649", variable: "#e45649",
    added: "#50a14f", removed: "#e45649"
  },
  {
    id: "solarized-light", name: "Solarized Light", scheme: "day",
    background: "#fdf6e3", foreground: "#657b83", lineNumber: "#93a1a1",
    comment: "#93a1a1", commentItalic: true,
    string: "#2aa198", number: "#d33682", keyword: "#859900", function: "#268bd2", type: "#cb4b16",
    meta: "#b58900", tag: "#268bd2", attribute: "#93a1a1", property: "#859900", variable: "#268bd2",
    added: "#859900", removed: "#dc322f"
  },
  {
    id: "catppuccin-latte", name: "Catppuccin Latte", scheme: "day",
    background: "#eff1f5", foreground: "#4c4f69", lineNumber: "#8c8fa1",
    comment: "#7c7f93", commentItalic: true,
    string: "#40a02b", number: "#fe640b", keyword: "#8839ef", function: "#1e66f5", type: "#df8e1d",
    meta: "#fe640b", tag: "#1e66f5", attribute: "#df8e1d", property: "#1e66f5", variable: "#4c4f69",
    added: "#40a02b", removed: "#d20f39"
  },
  {
    id: "gruvbox-light", name: "Gruvbox Light", scheme: "day",
    background: "#fbf1c7", foreground: "#3c3836", lineNumber: "#bdae93",
    comment: "#928374", commentItalic: true,
    string: "#79740e", number: "#8f3f71", keyword: "#9d0006", function: "#b57614", type: "#b57614",
    meta: "#af3a03", tag: "#427b58", attribute: "#b57614", property: "#689d6a", variable: "#076678",
    added: "#79740e", removed: "#cc241d"
  },
  {
    id: "ayu-light", name: "Ayu Light", scheme: "day",
    background: "#fcfcfc", foreground: "#5c6166", lineNumber: "#cbd0d7",
    comment: "#adaeb1", commentItalic: true,
    string: "#86b300", number: "#a37acc", keyword: "#fa8532", function: "#eba400", type: "#22a4e6",
    meta: "#e59645", tag: "#55b4d4", attribute: "#eba400", property: "#55b4d4", variable: "#5c6166",
    added: "#6cbf43", removed: "#ff7383"
  },
  {
    id: "tokyo-night-light", name: "Tokyo Night Light", scheme: "day",
    background: "#e6e7ed", foreground: "#343b59", lineNumber: "#9da0ab",
    comment: "#888b94", commentItalic: false,
    string: "#385f0d", number: "#965027", keyword: "#65359d", function: "#2959aa", type: "#343b58",
    meta: "#33635c", tag: "#8c4351", attribute: "#65359d", property: "#006c86", variable: "#343b58",
    added: "#33635c", removed: "#8c4351"
  },
  {
    id: "rose-pine-dawn", name: "Rosé Pine Dawn", scheme: "day",
    background: "#faf4ed", foreground: "#575279", lineNumber: "#797593",
    comment: "#9893a5", commentItalic: true,
    string: "#ea9d34", number: "#d7827e", keyword: "#286983", function: "#d7827e", type: "#56949f",
    meta: "#d7827e", tag: "#56949f", attribute: "#907aa9", property: "#56949f", variable: "#575279",
    added: "#56949f", removed: "#b4637a"
  },
  {
    id: "everforest-light", name: "Everforest Light", scheme: "day",
    background: "#fdf6e3", foreground: "#5c6a72", lineNumber: "#c5c8b8",
    comment: "#939f91", commentItalic: true,
    string: "#dfa000", number: "#df69ba", keyword: "#f85552", function: "#8da101", type: "#3a94c5",
    meta: "#35a77c", tag: "#f57d26", attribute: "#dfa000", property: "#f57d26", variable: "#5c6a72",
    added: "#8da101", removed: "#f85552"
  },
  {
    id: "night-owl-light", name: "Night Owl Light", scheme: "day",
    background: "#fbfbfb", foreground: "#403f53", lineNumber: "#90a7b2",
    comment: "#939dbb", commentItalic: false,
    string: "#c96765", number: "#aa0982", keyword: "#994cc3", function: "#4876d6", type: "#111111",
    meta: "#4876d6", tag: "#994cc3", attribute: "#4876d6", property: "#0c969b", variable: "#4876d6",
    added: "#08916a", removed: "#de3d3b"
  },
  {
    id: "vitesse-light", name: "Vitesse Light", scheme: "day",
    background: "#ffffff", foreground: "#393a34", lineNumber: "#c1c1bf",
    comment: "#a0ada0", commentItalic: false,
    string: "#b56959", number: "#2f798a", keyword: "#1e754f", function: "#59873a", type: "#2e8f82",
    meta: "#998418", tag: "#1e754f", attribute: "#b07d48", property: "#998418", variable: "#b07d48",
    added: "#22863a", removed: "#b31d28"
  },
  {
    id: "kanagawa-lotus", name: "Kanagawa Lotus", scheme: "day",
    background: "#f2ecbc", foreground: "#545464", lineNumber: "#766b90",
    comment: "#716e61", commentItalic: false,
    string: "#6f894e", number: "#b35b79", keyword: "#624c83", function: "#4d699b", type: "#597b75",
    meta: "#cc6d00", tag: "#77713f", attribute: "#624c83", property: "#597b75", variable: "#545464",
    added: "#6e915f", removed: "#d7474b"
  },
  {
    id: "material-lighter", name: "Material Lighter", scheme: "day",
    background: "#fafafa", foreground: "#90a4ae", lineNumber: "#cfd8dc",
    comment: "#90a4ae", commentItalic: true,
    string: "#91b859", number: "#f76d47", keyword: "#9c3eda", function: "#6182b8", type: "#e2931d",
    meta: "#39adb5", tag: "#e53935", attribute: "#9c3eda", property: "#e53935", variable: "#90a4ae",
    added: "#91b859", removed: "#e53935"
  },
  {
    id: "min-light", name: "Min Light", scheme: "day",
    background: "#ffffff", foreground: "#212121", lineNumber: "#cccccc",
    comment: "#c2c3c5", commentItalic: false,
    string: "#22863a", number: "#1976d2", keyword: "#d32f2f", function: "#6f42c1", type: "#6f42c1",
    meta: "#1976d2", tag: "#22863a", attribute: "#6f42c1", property: "#d32f2f", variable: "#212121",
    added: "#77cc00", removed: "#d32f2f"
  },
  {
    id: "snazzy-light", name: "Snazzy Light", scheme: "day",
    background: "#fafbfc", foreground: "#565869", lineNumber: "#b4b6c0",
    comment: "#adb1c2", commentItalic: false,
    string: "#cf9c00", number: "#ff5c57", keyword: "#f767bb", function: "#09a1ed", type: "#13bbb7",
    meta: "#11658f", tag: "#13bbb7", attribute: "#13bbb7", property: "#11658f", variable: "#565869",
    added: "#2dae58", removed: "#ff5c57"
  },
  {
    id: "github-dark", name: "GitHub Dark", scheme: "night",
    background: "#0d1117", foreground: "#e6edf3", lineNumber: "#6e7681",
    comment: "#8b949e", commentItalic: false,
    string: "#a5d6ff", number: "#79c0ff", keyword: "#ff7b72", function: "#d2a8ff", type: "#ffa657",
    meta: "#79c0ff", tag: "#7ee787", attribute: "#79c0ff", property: "#7ee787", variable: "#e6edf3",
    added: "#3fb950", removed: "#f85149"
  },
  {
    id: "github-dark-dimmed", name: "GitHub Dark Dimmed", scheme: "night",
    background: "#22272e", foreground: "#adbac7", lineNumber: "#636e7b",
    comment: "#768390", commentItalic: false,
    string: "#96d0ff", number: "#6cb6ff", keyword: "#f47067", function: "#dcbdfb", type: "#f69d50",
    meta: "#6cb6ff", tag: "#8ddb8c", attribute: "#6cb6ff", property: "#8ddb8c", variable: "#adbac7",
    added: "#57ab5a", removed: "#e5534b"
  },
  {
    id: "dark-plus", name: "Dark+", scheme: "night",
    background: "#1e1e1e", foreground: "#d4d4d4", lineNumber: "#858585",
    comment: "#6a9955", commentItalic: false,
    string: "#ce9178", number: "#b5cea8", keyword: "#569cd6", function: "#dcdcaa", type: "#4ec9b0",
    meta: "#c586c0", tag: "#569cd6", attribute: "#9cdcfe", property: "#9cdcfe", variable: "#9cdcfe",
    added: "#b5cea8", removed: "#ce9178"
  },
  {
    id: "one-dark-pro", name: "One Dark Pro", scheme: "night",
    background: "#282c34", foreground: "#abb2bf", lineNumber: "#495162",
    comment: "#7f848e", commentItalic: true,
    string: "#98c379", number: "#d19a66", keyword: "#c678dd", function: "#61afef", type: "#e5c07b",
    meta: "#d19a66", tag: "#e06c75", attribute: "#d19a66", property: "#e06c75", variable: "#e06c75",
    added: "#98c379", removed: "#e06c75"
  },
  {
    id: "dracula", name: "Dracula", scheme: "night",
    background: "#282a36", foreground: "#f8f8f2", lineNumber: "#6272a4",
    comment: "#6272a4", commentItalic: false,
    string: "#f1fa8c", number: "#bd93f9", keyword: "#ff79c6", function: "#50fa7b", type: "#8be9fd",
    meta: "#bd93f9", tag: "#ff79c6", attribute: "#50fa7b", property: "#8be9fd", variable: "#f8f8f2",
    added: "#50fa7b", removed: "#ff5555"
  },
  {
    id: "monokai", name: "Monokai", scheme: "night",
    background: "#272822", foreground: "#f8f8f2", lineNumber: "#90908a",
    comment: "#88846f", commentItalic: false,
    string: "#e6db74", number: "#ae81ff", keyword: "#f92672", function: "#a6e22e", type: "#a6e22e",
    meta: "#ae81ff", tag: "#f92672", attribute: "#a6e22e", property: "#66d9ef", variable: "#f8f8f2",
    added: "#a6e22e", removed: "#f92672"
  },
  {
    id: "solarized-dark", name: "Solarized Dark", scheme: "night",
    background: "#002b36", foreground: "#839496", lineNumber: "#586e75",
    comment: "#586e75", commentItalic: true,
    string: "#2aa198", number: "#d33682", keyword: "#859900", function: "#268bd2", type: "#cb4b16",
    meta: "#b58900", tag: "#268bd2", attribute: "#93a1a1", property: "#859900", variable: "#268bd2",
    added: "#859900", removed: "#dc322f"
  },
  {
    id: "nord", name: "Nord", scheme: "night",
    background: "#2e3440", foreground: "#d8dee9", lineNumber: "#4c566a",
    comment: "#616e88", commentItalic: false,
    string: "#a3be8c", number: "#b48ead", keyword: "#81a1c1", function: "#88c0d0", type: "#8fbcbb",
    meta: "#5e81ac", tag: "#81a1c1", attribute: "#8fbcbb", property: "#8fbcbb", variable: "#d8dee9",
    added: "#a3be8c", removed: "#bf616a"
  },
  {
    id: "catppuccin-mocha", name: "Catppuccin Mocha", scheme: "night",
    background: "#1e1e2e", foreground: "#cdd6f4", lineNumber: "#7f849c",
    comment: "#9399b2", commentItalic: true,
    string: "#a6e3a1", number: "#fab387", keyword: "#cba6f7", function: "#89b4fa", type: "#f9e2af",
    meta: "#fab387", tag: "#89b4fa", attribute: "#f9e2af", property: "#89b4fa", variable: "#cdd6f4",
    added: "#a6e3a1", removed: "#f38ba8"
  },
  {
    id: "catppuccin-macchiato", name: "Catppuccin Macchiato", scheme: "night",
    background: "#24273a", foreground: "#cad3f5", lineNumber: "#8087a2",
    comment: "#939ab7", commentItalic: true,
    string: "#a6da95", number: "#f5a97f", keyword: "#c6a0f6", function: "#8aadf4", type: "#eed49f",
    meta: "#f5a97f", tag: "#8aadf4", attribute: "#eed49f", property: "#8aadf4", variable: "#cad3f5",
    added: "#a6da95", removed: "#ed8796"
  },
  {
    id: "catppuccin-frappe", name: "Catppuccin Frappé", scheme: "night",
    background: "#303446", foreground: "#c6d0f5", lineNumber: "#838ba7",
    comment: "#949cbb", commentItalic: true,
    string: "#a6d189", number: "#ef9f76", keyword: "#ca9ee6", function: "#8caaee", type: "#e5c890",
    meta: "#ef9f76", tag: "#8caaee", attribute: "#e5c890", property: "#8caaee", variable: "#c6d0f5",
    added: "#a6d189", removed: "#e78284"
  },
  {
    id: "gruvbox-dark", name: "Gruvbox Dark", scheme: "night",
    background: "#282828", foreground: "#ebdbb2", lineNumber: "#665c54",
    comment: "#928374", commentItalic: true,
    string: "#b8bb26", number: "#d3869b", keyword: "#fb4934", function: "#fabd2f", type: "#fabd2f",
    meta: "#fe8019", tag: "#8ec07c", attribute: "#fabd2f", property: "#689d6a", variable: "#83a598",
    added: "#b8bb26", removed: "#fb4934"
  },
  {
    id: "ayu-dark", name: "Ayu Dark", scheme: "night",
    background: "#10141c", foreground: "#bfbdb6", lineNumber: "#404758",
    comment: "#5a6673", commentItalic: true,
    string: "#aad94c", number: "#d2a6ff", keyword: "#ff8f40", function: "#ffb454", type: "#59c2ff",
    meta: "#e6c08a", tag: "#39bae6", attribute: "#ffb454", property: "#39bae6", variable: "#bfbdb6",
    added: "#70bf56", removed: "#f26d78"
  },
  {
    id: "ayu-mirage", name: "Ayu Mirage", scheme: "night",
    background: "#242936", foreground: "#cccac2", lineNumber: "#4a5261",
    comment: "#6e7c8f", commentItalic: true,
    string: "#d5ff80", number: "#dfbfff", keyword: "#ffa659", function: "#ffcd66", type: "#73d0ff",
    meta: "#d9be98", tag: "#5ccfe6", attribute: "#ffcd66", property: "#5ccfe6", variable: "#cccac2",
    added: "#87d96c", removed: "#f27983"
  },
  {
    id: "tokyo-night", name: "Tokyo Night", scheme: "night",
    background: "#1a1b26", foreground: "#a9b1d6", lineNumber: "#363b54",
    comment: "#51597d", commentItalic: false,
    string: "#9ece6a", number: "#ff9e64", keyword: "#bb9af7", function: "#7aa2f7", type: "#c0caf5",
    meta: "#73daca", tag: "#f7768e", attribute: "#bb9af7", property: "#0db9d7", variable: "#c0caf5",
    added: "#449dab", removed: "#914c54"
  },
  {
    id: "tokyo-night-storm", name: "Tokyo Night Storm", scheme: "night",
    background: "#24283b", foreground: "#a9b1d6", lineNumber: "#3b4261",
    comment: "#5f6996", commentItalic: false,
    string: "#9ece6a", number: "#ff9e64", keyword: "#bb9af7", function: "#7aa2f7", type: "#c0caf5",
    meta: "#73daca", tag: "#f7768e", attribute: "#bb9af7", property: "#2ac3de", variable: "#c0caf5",
    added: "#449dab", removed: "#914c54"
  },
  {
    id: "rose-pine", name: "Rosé Pine", scheme: "night",
    background: "#191724", foreground: "#e0def4", lineNumber: "#908caa",
    comment: "#6e6a86", commentItalic: true,
    string: "#f6c177", number: "#ebbcba", keyword: "#31748f", function: "#ebbcba", type: "#9ccfd8",
    meta: "#ebbcba", tag: "#9ccfd8", attribute: "#c4a7e7", property: "#9ccfd8", variable: "#e0def4",
    added: "#9ccfd8", removed: "#eb6f92"
  },
  {
    id: "rose-pine-moon", name: "Rosé Pine Moon", scheme: "night",
    background: "#232136", foreground: "#e0def4", lineNumber: "#908caa",
    comment: "#6e6a86", commentItalic: true,
    string: "#f6c177", number: "#ea9a97", keyword: "#3e8fb0", function: "#ea9a97", type: "#9ccfd8",
    meta: "#ea9a97", tag: "#9ccfd8", attribute: "#c4a7e7", property: "#9ccfd8", variable: "#e0def4",
    added: "#9ccfd8", removed: "#eb6f92"
  },
  {
    id: "everforest-dark", name: "Everforest Dark", scheme: "night",
    background: "#2d353b", foreground: "#d3c6aa", lineNumber: "#606a64",
    comment: "#859289", commentItalic: true,
    string: "#dbbc7f", number: "#d699b6", keyword: "#e67e80", function: "#a7c080", type: "#7fbbb3",
    meta: "#83c092", tag: "#e69875", attribute: "#dbbc7f", property: "#e69875", variable: "#d3c6aa",
    added: "#a7c080", removed: "#e67e80"
  },
  {
    id: "night-owl", name: "Night Owl", scheme: "night",
    background: "#011627", foreground: "#d6deeb", lineNumber: "#4b6479",
    comment: "#637777", commentItalic: false,
    string: "#ecc48d", number: "#f78c6c", keyword: "#c792ea", function: "#82aaff", type: "#ffcb8b",
    meta: "#82aaff", tag: "#caece6", attribute: "#c5e478", property: "#7fdbca", variable: "#c5e478",
    added: "#22da6e", removed: "#ef5350"
  },
  {
    id: "material-palenight", name: "Material Palenight", scheme: "night",
    background: "#292d3e", foreground: "#babed8", lineNumber: "#3a3f58",
    comment: "#676e95", commentItalic: true,
    string: "#c3e88d", number: "#f78c6c", keyword: "#c792ea", function: "#82aaff", type: "#ffcb6b",
    meta: "#89ddff", tag: "#f07178", attribute: "#c792ea", property: "#f07178", variable: "#babed8",
    added: "#c3e88d", removed: "#f07178"
  },
  {
    id: "material-ocean", name: "Material Ocean", scheme: "night",
    background: "#0f111a", foreground: "#babed8", lineNumber: "#252836",
    comment: "#464b5d", commentItalic: true,
    string: "#c3e88d", number: "#f78c6c", keyword: "#c792ea", function: "#82aaff", type: "#ffcb6b",
    meta: "#89ddff", tag: "#f07178", attribute: "#c792ea", property: "#f07178", variable: "#babed8",
    added: "#c3e88d", removed: "#f07178"
  },
  {
    id: "material-darker", name: "Material Darker", scheme: "night",
    background: "#212121", foreground: "#eeffff", lineNumber: "#424242",
    comment: "#545454", commentItalic: true,
    string: "#c3e88d", number: "#f78c6c", keyword: "#c792ea", function: "#82aaff", type: "#ffcb6b",
    meta: "#89ddff", tag: "#f07178", attribute: "#c792ea", property: "#f07178", variable: "#eeffff",
    added: "#c3e88d", removed: "#f07178"
  },
  {
    id: "kanagawa-wave", name: "Kanagawa Wave", scheme: "night",
    background: "#1f1f28", foreground: "#dcd7ba", lineNumber: "#54546d",
    comment: "#727169", commentItalic: false,
    string: "#98bb6c", number: "#d27e99", keyword: "#957fb8", function: "#7e9cd8", type: "#7aa89f",
    meta: "#ffa066", tag: "#e6c384", attribute: "#957fb8", property: "#7aa89f", variable: "#dcd7ba",
    added: "#76946a", removed: "#c34043"
  },
  {
    id: "kanagawa-dragon", name: "Kanagawa Dragon", scheme: "night",
    background: "#181616", foreground: "#c5c9c5", lineNumber: "#625e5a",
    comment: "#737c73", commentItalic: false,
    string: "#8a9a7b", number: "#a292a3", keyword: "#8992a7", function: "#8ba4b0", type: "#8ea4a2",
    meta: "#b6927b", tag: "#c4b28a", attribute: "#8992a7", property: "#8ea4a2", variable: "#c5c9c5",
    added: "#76946a", removed: "#c34043"
  },
  {
    id: "synthwave-84", name: "Synthwave '84", scheme: "night",
    background: "#262335", foreground: "#bbbbbb", lineNumber: "#888690",
    comment: "#848bbd", commentItalic: true,
    string: "#ff8b39", number: "#f97e72", keyword: "#fede5d", function: "#36f9f6", type: "#fe4450",
    meta: "#f97e72", tag: "#72f1b8", attribute: "#fede5d", property: "#72f1b8", variable: "#ff7edb",
    added: "#63c89e", removed: "#fe4450"
  },
  {
    id: "vitesse-dark", name: "Vitesse Dark", scheme: "night",
    background: "#121212", foreground: "#cecabe", lineNumber: "#52514f",
    comment: "#687668", commentItalic: false,
    string: "#c98a7d", number: "#4c9a91", keyword: "#4d9375", function: "#80a665", type: "#5da994",
    meta: "#b8a965", tag: "#4d9375", attribute: "#bd976a", property: "#b8a965", variable: "#bd976a",
    added: "#4d9375", removed: "#cb7676"
  },
  {
    id: "vesper", name: "Vesper", scheme: "night",
    background: "#101010", foreground: "#ffffff", lineNumber: "#505050",
    comment: "#575757", commentItalic: false,
    string: "#99ffe4", number: "#ffc799", keyword: "#a0a0a0", function: "#ffc799", type: "#ffc799",
    meta: "#a0a0a0", tag: "#ffc799", attribute: "#a0a0a0", property: "#ffc799", variable: "#ffffff",
    added: "#99ffe4", removed: "#ff8080"
  },
  {
    id: "houston", name: "Houston", scheme: "night",
    background: "#17191e", foreground: "#eef0f9", lineNumber: "#545864",
    comment: "#909299", commentItalic: true,
    string: "#ffd493", number: "#ffd493", keyword: "#54b9ff", function: "#00daef", type: "#acafff",
    meta: "#ffd493", tag: "#54b9ff", attribute: "#4bf3c8", property: "#4bf3c8", variable: "#4bf3c8",
    added: "#4bf3c8", removed: "#f4587e"
  },
  {
    id: "andromeeda", name: "Andromeeda", scheme: "night",
    background: "#23262e", foreground: "#d5ced9", lineNumber: "#746f77",
    comment: "#87888f", commentItalic: false,
    string: "#96e072", number: "#f39c12", keyword: "#c74ded", function: "#ffe66d", type: "#ffe66d",
    meta: "#ee5d43", tag: "#f92672", attribute: "#ffe66d", property: "#c74ded", variable: "#00e8c6",
    added: "#96e072", removed: "#ee5d43"
  }
];
