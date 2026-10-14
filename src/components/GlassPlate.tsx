/**
 * The baked glass under one glass surface (`backdrop.css`, `lib/glassPlate.ts`).
 *
 * Glass is the window's background, blurred, so it has to lie where that background lies
 * however the surface over it moves. The inner layer is the size of the window and fixed to
 * it, fitted as the background is; the outer one is the surface's shape, and its clip path
 * cuts the inner one to it (`overflow` does not clip a fixed box).
 *
 * A `background-attachment: fixed` reads the same in a single box. With more than one such
 * background on a page, though, Chromium stops presenting any scroller's scroll from the
 * compositor: every step of a scroll waits for the main thread, and the timeline stuttered
 * whenever that thread was busy.
 *
 * The inner layer is fixed to the window only while no ancestor is a containing block for
 * fixed boxes (a transform, a filter, `contain`). It takes no box unless the panes are glass.
 */
export function GlassPlate() {
  return (
    <span className="glass-plate" aria-hidden="true">
      <span className="glass-plate__picture" />
    </span>
  );
}
