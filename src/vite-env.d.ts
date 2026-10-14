/// <reference types="vite/client" />

declare module "*.css";

/** The file half of Material Icon Theme's manifest, distilled by `scripts/vite-file-icons.mjs`. */
declare module "virtual:mewrk-file-icons" {
  const manifest: {
    /** Icon of a file nothing below matches. */
    fallback: string;
    /** Lowercased file name, optionally led by its parent directories, to icon. */
    names: Record<string, string>;
    /** Lowercased extension, which may itself contain dots, to icon. */
    extensions: Record<string, string>;
    /** Icons that also come as `<icon>_light`, drawn for light backgrounds. */
    light: string[];
    /** The most path segments any key of `names` has. */
    nameDepth: number;
  };
  export default manifest;
}

/** pdf.js ships no declarations for its worker; the viewer only hands the module back to pdf.js. */
declare module "pdfjs-dist/legacy/build/pdf.worker.mjs" {
  export const WorkerMessageHandler: unknown;
}
