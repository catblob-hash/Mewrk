import { CAT_COMPOSER_BODY, CAT_COMPOSER_VIEWBOX, CAT_FACE, CAT_LAPTOP, CAT_ON_LEDGE, CAT_TAIL_RAISED } from "./catArt";

/**
 * The cat loafing on the top rim of a draft composer, with its laptop.
 *
 * This is the mascot's own drawing (`catArt.ts`), not the brand mark. The original drawing
 * puts the cat on a desk; here the composer's own top border is the desk, so the cat is
 * lowered onto the border line instead of a slab. That is what sells the composer as the
 * thing the cat is lying on rather than something it floats above — and why it draws its own
 * rear half: the drawing's underside stops short of the desk, and its tail hangs over the
 * desk's front edge, which on the composer would be hanging into the box.
 *
 * ## The coordinate facts the CSS depends on
 *
 * The viewBox is the artwork's own drawing space ({@link CAT_COMPOSER_VIEWBOX}), so every
 * number in `orchestration.css` — the transform origins especially — is a drawing coordinate
 * and can be read straight off `catArt.ts`. The ledge line is y=730.9, and the box is padded
 * round the cat for the breath and the tail's swing; both are explained where they are
 * defined.
 *
 * ## Why the cat and the laptop are one path
 *
 * They overlap, and under `fill-rule="evenodd"` the overlap cancels — that white notch is the
 * gap that reads as the far paw resting on the keyboard. Drawing them as two filled shapes
 * fills the notch in and the paw disappears into the laptop. The face is punched out of the
 * same path for the same reason, which is also why the head cannot be animated separately
 * here: anything drawn over the head to nod it would plug the eyes. The figure breathes and
 * the tail sways instead; the tail is a path of its own for that, under the body so that its
 * root disappears into the rump.
 */
export function ComposerCat() {
  return (
    <svg className="composer-cat" viewBox={CAT_COMPOSER_VIEWBOX} aria-hidden="true">
      <g className="composer-cat__figure">
        <g transform={CAT_ON_LEDGE}>
          <path className="composer-cat__tail" d={CAT_TAIL_RAISED} />
          <path className="composer-cat__body" fillRule="evenodd" d={`${CAT_COMPOSER_BODY}${CAT_LAPTOP}${CAT_FACE}`} />
        </g>
      </g>
    </svg>
  );
}
