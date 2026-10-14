import { fileIconName, fileIconUrl, lightFileIconName } from "../lib/fileIcons";
import "./FileIcon.css";

/**
 * The icon of a file's type, chosen from its name.
 *
 * An `<img>` rather than inline markup: the icons are third-party drawings fetched
 * on demand, and the page's style policy does not reach inside an image. The few
 * icons that have a second drawing for light backgrounds render both and let the
 * theme attribute pick, so nothing here has to watch the theme.
 */
export function FileIcon({
  path,
  size = 14,
  inline = false,
  className
}: {
  path: string;
  size?: number;
  /** Set in a line of text, ahead of the name it belongs to, rather than as a box of a row. */
  inline?: boolean;
  className?: string;
}) {
  const icon = fileIconName(path);
  const light = lightFileIconName(icon);
  const image = (name: string, variant?: "day" | "night") => (
    <img
      className={["file-icon", inline && "file-icon--inline", variant && `file-icon--${variant}`, className]
        .filter(Boolean)
        .join(" ")}
      src={fileIconUrl(name)}
      width={size}
      height={size}
      alt=""
      aria-hidden="true"
      draggable={false}
    />
  );
  if (!light) return image(icon);
  return (
    <>
      {image(icon, "night")}
      {image(light, "day")}
    </>
  );
}
