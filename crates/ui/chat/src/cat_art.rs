//! The mascot beside a streaming round (upstream's `catArt.ts` and the `.stream-cat-*` rules of `conversation.css`): the cat
//! lying chin-down at its laptop, as path data, and the loops that move it.
//!
//! The drawing is one mass with the face punched out of it under even-odd, and each moving part is freed differently: both
//! forepaws are cut out along a line their own movement cannot open, while the pupils and lids are drawn over the head in its
//! own colour and show only where they cross an eye hole. That is why this paints one coverage mask and not a set of shapes:
//! one colour over itself is invisible, so a union is all the drawing ever asks for.
//!
//! Fluent has no path fill, so a frame is scan-converted into coverage here and handed to `ui.fill` one device pixel run at a
//! time. The cat is one text line tall, so that is at most a few hundred rects.

use std::sync::LazyLock;

use fluent::ui::{rect, Rect, Ui};
use fluent::Color;

/// `left top width height` of the scene, padded because a breath scales the figure about the ledge: at 1.035 the ears reach
/// y=348.7 and the tail's tip y=948.7, so a box fitted to the neutral pose would clip both ends once a breath.
pub const CAT_SCENE_VIEWBOX: (f32, f32, f32, f32) = (292.0, 344.0, 724.0, 610.0);
/// Where the tail hangs from: the underside of the desk the drawing puts the cat on.
pub const CAT_LEDGE_Y: f32 = 730.9;
/// `CAT_ON_LEDGE`: the slab's thickness, which brings what sits on the desk down to where the tail hangs from.
pub const CAT_ON_LEDGE_Y: f32 = 62.12;
/// Where the far forearm bends: the centre of the arc `CAT_FAR_PAW` is cut along, and so the one point it can turn about
/// without its joint opening.
pub const CAT_WRIST: (f32, f32) = (688.0, 624.0);
/// How far `CAT_NEAR_PAW` reaches back into the body past the cut. Its slide must stay inside this.
pub const CAT_NEAR_PAW_OVERLAP: f32 = 28.3;
/// How far above its eye each lid is parked. Lowered this far, a lid's edge is its eye's own lower curve.
pub const CAT_LID_LIFT: f32 = 54.0;
/// A closed lid goes this far past the eye, or the eye's outline stays as a ghost.
pub const LID_OVERSHOOT: f32 = 8.0;

/// `.stream-cat-figure { transform-origin }`.
const FIGURE_ORIGIN_Y: f32 = 730.9;
/// `.stream-cat-tail { transform-origin }`.
const TAIL_ORIGIN: (f32, f32) = (431.6, 730.9);
/// `<rect className="stream-cat-ledge">`: one pixel thick at the 16 px the stylesheet draws the cat.
const LEDGE: (f32, f32, f32, f32) = (300.0, CAT_LEDGE_Y, 690.0, 38.125);
const LEDGE_RADIUS: f32 = 19.0625;

pub const CAT_BODY: &str = "M463.2 299.9C451.3 310 447.9 323.9 444.8 339.2C438.6 369.6 438.7 400 440.9 430.9C427.3 446.8 416.1 464.9 408.3 484.5C404.6 493.8 402 504 398.8 513.1C399.5 497.4 403.4 482.8 407.6 468C392.4 473.9 378.2 483.8 365.9 494.5C321.1 533.6 291.7 598.7 309.5 657.8C370.1 658.6 430.8 657.5 491.5 658.1C500.7 671.3 515.1 674.8 527.6 663.5C530.6 666 533.2 668.7 537.1 669.7C553.3 674.1 563.6 658.1 562.3 643.8C559.5 614.1 519.6 607.1 496.6 606.9C491.1 606.9 485.6 607.2 480.2 607.6C472.9 608.2 465.3 609.6 458.3 610.5C482.7 595.3 520.2 593.7 545.7 607.8C558.3 614.7 570 627 571.8 641.9C572.4 647.3 571.4 652.8 570.4 657.8C592.4 659 614.7 658.2 636.7 657.9C637.6 655.1 638.2 650.9 640.3 648.3C649.1 637.5 667.2 641.2 679.1 641.2C708.7 641.1 739 643.1 768.5 640.9C770.8 636.4 772.1 630.9 773.4 626C771.7 623 770.8 620.2 768.4 617.5C755.6 602.7 732.1 605.8 715.4 607.5C730 598.1 747.2 597.9 762.9 604.4C767 599.5 770.2 593.8 773.9 588.8C776 589.1 785.1 591.7 786.6 590.8C787.7 590.1 787.9 587.6 788.3 586.5C788.5 585.7 789.8 583.2 789.6 582.5C788.9 580.1 780.6 579.3 778.4 578.8C779.1 576.2 779.6 569.2 781.8 567.6C785.2 565 792.6 568.1 795.6 565.4C797.1 564.1 798 559.3 798.7 557.4C795.9 557.1 792 556.5 788.6 556.6C787.6 556.6 785.2 557.3 784.4 556.5C783.2 555.4 785.3 547.2 785.5 545.4C786.5 536 786.1 526.1 784.9 516.7C781.2 488.3 770 465.1 756.3 440.6C768.6 414.2 780.1 386.8 784.1 357.7C785.7 345.4 787.7 331.7 778.5 322.5C763.8 323.1 750.1 328.7 736.9 335.6C713.3 347.8 692.9 364.3 672.5 381.2C659.1 377.7 646.1 373.1 632.2 371.3C609 368.2 585.8 370.4 563 374.7C537 345.3 505.4 307.8 465.1 299.5C464 299.6 464.6 299.5 463.2 299.9Z";
pub const CAT_LAPTOP: &str = "M774.9 650.2C743.4 651.5 711.3 650.3 679.6 650.3C677.3 650.3 675 650.3 672.7 650.3C666.8 650.3 651.3 647.3 647.5 654.2C645.9 657.1 646.9 658.9 646.2 661.8C647.5 663.4 647.8 665.1 649.6 666.4C654.4 669.9 663.7 667.9 669.1 667.9C683.4 667.9 697.7 667.9 712.1 667.9C762.5 667.9 812.9 667.4 863.3 667.2C880.3 667.2 897.3 667.1 914.3 667.1C922.4 667 931.2 667.9 939.3 666.8C940 665.9 942.3 665.8 943.4 665.3C945.9 664.1 947.8 662.5 949.6 660.4C952.3 657.5 953.1 652.4 954.3 648.7C957.1 640.8 960.2 633.1 962.9 625.2C972.7 597.5 983.4 570.2 993 542.5C996.9 531.4 1009.8 509.2 998.6 499.7C990.8 493.2 977.5 496 968.3 496C947 496 925.7 496 904.4 496C889 496 873.5 496.1 858 496.1C849.5 496.1 840.7 495.1 833.1 499.8C828 503.1 826.6 508.4 824.8 513.8C821.7 523 818.1 532.2 814.8 541.4C801.7 577.5 790.2 615.1 774.9 650.2Z";
pub const CAT_TAIL: &str = "M396.8 731.2C397.9 755.4 404.2 777.3 417.4 797.5C426.7 811.8 445.4 826.8 446.1 845.2C446.7 861.6 431.7 857.1 421.1 862.2C402.4 871.3 395.7 893.5 403.3 912.4C415.5 942.4 454.7 947.2 481.4 935.5C503.8 925.7 518.9 904.7 523.7 881.1C530 850.2 517.1 819.1 498.6 794.9C484.9 777.1 461.5 755.6 466.4 730.9Z";
pub const CAT_EYES: &str = "M507.6 522.6C506.9 529.9 507.1 536.2 509.6 543.4C520.5 575.2 567.7 584.5 592.7 564.4C602.6 556.4 607.3 547.3 609.4 535.2C602.7 534.1 595.7 533.8 589 533.8C588.7 539.8 589 548.1 585.2 553.4C578.9 562 566.7 562.9 561.2 552.7C557.5 545.8 558.6 537.6 558.6 530.3C541.8 527.6 524.9 525.3 508.1 522.5C507.9 522.6 507.7 522.6 507.6 522.6ZM755.1 525.4C749.2 526.7 743.2 527.8 737.4 529C737.1 537.2 738 547.2 733 554.4C727.2 562.7 716.4 561.5 710.9 553.7C707.1 548.3 708.2 539.1 708 533C697.2 533.6 686 534.4 675.3 535.4C676.1 544.7 678 553.7 684.6 560.9C701.9 579.6 732.7 577.7 748.5 558.1C756.1 548.7 757.7 536.9 757.1 525.4C756.2 525 756 525.2 755.1 525.4Z";
pub const CAT_WHISKERS: &str = "M423 559.9C423.8 560.2 424.4 562 426.1 562.3C429.6 563 434.2 560.9 437.6 560.4C443.3 559.4 449.1 558.9 454.9 558.9C462 558.9 470.9 562.2 477.8 560.9C478.9 560.7 479.2 558.9 480 558.6C479.3 558 479.3 555.7 477.7 554.7C475.4 553.2 471.4 553.1 468.7 552.6C458 550.6 446.8 551 436.1 552.8C432.8 553.4 427.6 553.8 424.8 555.8C423 557.1 423.9 559 423 559.9ZM429.4 592.8C430.4 593 431 594.7 432.5 594.9C435.6 595.2 440.2 591.5 443 590.2C449.1 587.4 455.7 584.9 462.2 583.3C466.8 582.1 475.2 582.4 479 579.8C480.1 579 480.3 577.1 481 576.6C480.4 576.4 479.9 574.3 478.7 573.9C474.5 572.6 468.9 574.1 464.7 574.9C455.8 576.7 447.2 579.7 439.1 583.8C436.3 585.2 432.2 586.6 430.2 589.2C429.4 590.3 429.9 592.1 429.4 592.8Z";
pub const CAT_MUZZLE: &str = "M634.8 577C635.6 581.6 638.3 582.5 642.2 584.8C642.4 587.3 643.7 590.9 642.3 593.6C641.7 594.8 639.4 595.6 638.2 596.3C634.8 598.7 631.6 601.2 628.4 603.8C626.8 605.1 623.4 606.8 623.2 609.1C623.1 611.1 624.5 611.5 624.7 612.9C625.5 612.5 627 613.3 628 612.9C630.5 611.8 632.9 608.6 635.1 606.8C638.8 603.7 642.8 601.1 646.8 598.7C651 601.7 655 604.9 658.9 608.5C660.4 609.9 661.6 612.3 663.6 613.1C664.6 613.5 665.9 612.6 666.6 612.8C666.8 611.1 668.3 610.9 667.8 608.7C666.7 604.5 659.4 600 655.9 597.3C654.6 596.3 651.5 595.1 651.1 593.6C650.8 592.7 650.9 591.6 650.9 590.7C650.8 589.3 650.3 586.3 651.1 585C652.3 582.9 656.2 582.8 657.7 580.6C659 578.8 657.4 577.3 657.9 576C652.6 572.6 644.4 574.3 638.1 574.3C636.5 574.5 635.5 575.8 634.8 577Z";
pub const CAT_EYE_WHITES: &str = "M507.6 522.6C506.9 529.9 507.1 536.2 509.6 543.4C520.5 575.2 567.7 584.5 592.7 564.4C602.6 556.4 607.3 547.3 609.4 535.2C602.7 534.1 595.7 533.8 589 533.8L558.6 530.3C541.8 527.6 524.9 525.3 508.1 522.5C507.9 522.6 507.7 522.6 507.6 522.6ZM755.1 525.4C749.2 526.7 743.2 527.8 737.4 529L708 533C697.2 533.6 686 534.4 675.3 535.4C676.1 544.7 678 553.7 684.6 560.9C701.9 579.6 732.7 577.7 748.5 558.1C756.1 548.7 757.7 536.9 757.1 525.4C756.2 525 756 525.2 755.1 525.4Z";
pub const CAT_PUPILS: &str = "M589 533.8C588.7 539.8 589 548.1 585.2 553.4C578.9 562 566.7 562.9 561.2 552.7C557.5 545.8 558.6 537.6 558.6 530.3L558.6 505L589 505ZM737.4 529C737.1 537.2 738 547.2 733 554.4C727.2 562.7 716.4 561.5 710.9 553.7C707.1 548.3 708.2 539.1 708 533L708 505L737.4 505Z";
pub const CAT_LIDS: &str = "M500 468.6C499.2 475.9 499.4 482.2 502.3 489.4C514.8 521.2 569.1 530.5 597.8 510.4C609.2 502.4 614.6 493.3 617 481.2L617 451L500 451ZM669.2 481.4C670.1 490.7 672.3 499.7 679.9 506.9C699.8 525.6 735.2 523.7 753.3 504.1C762.1 494.7 763.9 482.9 763.2 471.4L750 451L669.2 451Z";
const DESK_UNDER_NEAR_PAW: &str = "C370.1 658.6 430.8 657.5 491.5 658.1";
const CHIN_OVER_NEAR_PAW: &str = "C482.7 595.3 520.2 593.7 545.7 607.8";
const FAR_PAW_UNDERSIDE: &str = "C708.7 641.1 739 643.1 768.5 640.9";
const CHEEK_OVER_FAR_PAW: &str = "C730 598.1 747.2 597.9 762.9 604.4";

/// Splits a path around one of its own segments. Panics rather than cut somewhere else; the tests build every path.
fn cut_around<'a>(path: &'a str, segment: &str) -> (&'a str, &'a str) {
    path.split_once(segment).unwrap_or_else(|| panic!("catArt: the drawing has no segment {segment}"))
}

/// The paths the streaming cat cuts out of `CAT_BODY`. See upstream's comments in `catArt.ts` for why each cut has its shape.
pub struct CatCuts {
    pub body_without_paws: String,
    pub far_paw: String,
    pub near_paw: String,
}

pub static CAT_CUTS: LazyLock<CatCuts> = LazyLock::new(|| {
    let (body_up_to_near_paw, near_paw_onwards) = cut_around(CAT_BODY, DESK_UNDER_NEAR_PAW);
    let (near_paw_outline, body_between_paws) = cut_around(near_paw_onwards, CHIN_OVER_NEAR_PAW);
    let (body_up_to_far_paw, far_paw_onwards) = cut_around(body_between_paws, FAR_PAW_UNDERSIDE);
    let (far_paw_outline, body_past_far_paw) = cut_around(far_paw_onwards, CHEEK_OVER_FAR_PAW);
    CatCuts {
        body_without_paws: format!(
            "{body_up_to_near_paw}C359.1 658.5 408.7 657.8 458.3 657.9L458.3 610.5{CHIN_OVER_NEAR_PAW}{body_up_to_far_paw}C687.5 641.2 696 641.3 704.4 641.5A24 24 0 0 0 700 603.2L715.4 607.5{CHEEK_OVER_FAR_PAW}{body_past_far_paw}"
        ),
        far_paw: format!("M699.8 641.4C722.7 641.8 745.8 642.6 768.5 640.9{far_paw_outline}L700 596L688 603A21 21 0 0 1 699.8 641.4Z"),
        near_paw: format!("M430 657.9C450.5 657.9 471 657.9 491.5 658.1{near_paw_outline}L430 606Z"),
    }
});

// ---------------------------------------------------------------- flattening

type Polyline = Vec<[f32; 2]>;

const CUBIC_STEPS: usize = 10;
const ARC_STEPS: usize = 18;

/// The drawing's paths use absolute `M`, `L`, `C`, `A` and `Z`, which is all this reads.
pub fn flatten(path: &str) -> Vec<Polyline> {
    let bytes = path.as_bytes();
    let mut at = 0;
    let number = |at: &mut usize| -> f32 {
        while *at < bytes.len() && (bytes[*at] == b' ' || bytes[*at] == b',') {
            *at += 1;
        }
        let start = *at;
        while *at < bytes.len() && (bytes[*at].is_ascii_digit() || bytes[*at] == b'.' || bytes[*at] == b'-') {
            *at += 1;
        }
        path[start..*at].parse().unwrap_or(0.0)
    };
    let mut out: Vec<Polyline> = Vec::new();
    let mut current: Polyline = Vec::new();
    let mut pen = [0.0f32; 2];
    while at < bytes.len() {
        let command = bytes[at];
        at += 1;
        match command {
            b'M' => {
                if current.len() > 1 {
                    out.push(std::mem::take(&mut current));
                }
                current.clear();
                pen = [number(&mut at), number(&mut at)];
                current.push(pen);
            }
            b'L' => {
                pen = [number(&mut at), number(&mut at)];
                current.push(pen);
            }
            b'C' => {
                let (c1, c2, end) = ([number(&mut at), number(&mut at)], [number(&mut at), number(&mut at)], [number(&mut at), number(&mut at)]);
                for step in 1..=CUBIC_STEPS {
                    let t = step as f32 / CUBIC_STEPS as f32;
                    let u = 1.0 - t;
                    let axis = |i: usize| u * u * u * pen[i] + 3.0 * u * u * t * c1[i] + 3.0 * u * t * t * c2[i] + t * t * t * end[i];
                    current.push([axis(0), axis(1)]);
                }
                pen = end;
            }
            b'A' => {
                let (rx, ry, rotation, large, sweep) = (number(&mut at), number(&mut at), number(&mut at), number(&mut at), number(&mut at));
                let end = [number(&mut at), number(&mut at)];
                flatten_arc(&mut current, pen, rx, ry, rotation, large != 0.0, sweep != 0.0, end);
                pen = end;
            }
            b'Z' => {
                if current.len() > 1 {
                    out.push(std::mem::take(&mut current));
                }
                current.clear();
            }
            _ => {}
        }
    }
    if current.len() > 1 {
        out.push(current);
    }
    out
}

/// SVG's endpoint arc, through its centre parameterisation (SVG 2, F.6.5).
#[allow(clippy::too_many_arguments)]
fn flatten_arc(out: &mut Polyline, from: [f32; 2], rx: f32, ry: f32, rotation: f32, large: bool, sweep: bool, to: [f32; 2]) {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 {
        out.push(to);
        return;
    }
    let (sin, cos) = rotation.to_radians().sin_cos();
    let (dx, dy) = ((from[0] - to[0]) * 0.5, (from[1] - to[1]) * 0.5);
    let (x1, y1) = (cos * dx + sin * dy, -sin * dx + cos * dy);
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        let grow = lambda.sqrt();
        rx *= grow;
        ry *= grow;
    }
    let (rx2, ry2, x12, y12) = (rx * rx, ry * ry, x1 * x1, y1 * y1);
    let factor = (((rx2 * ry2 - rx2 * y12 - ry2 * x12) / (rx2 * y12 + ry2 * x12)).max(0.0)).sqrt() * if large == sweep { -1.0 } else { 1.0 };
    let (cx1, cy1) = (factor * rx * y1 / ry, -factor * ry * x1 / rx);
    let (cx, cy) = (cos * cx1 - sin * cy1 + (from[0] + to[0]) * 0.5, sin * cx1 + cos * cy1 + (from[1] + to[1]) * 0.5);
    let angle = |ux: f32, uy: f32| uy.atan2(ux);
    let start = angle((x1 - cx1) / rx, (y1 - cy1) / ry);
    let mut delta = angle((-x1 - cx1) / rx, (-y1 - cy1) / ry) - start;
    if sweep && delta < 0.0 {
        delta += std::f32::consts::TAU;
    } else if !sweep && delta > 0.0 {
        delta -= std::f32::consts::TAU;
    }
    for step in 1..=ARC_STEPS {
        let a = start + delta * step as f32 / ARC_STEPS as f32;
        let (x, y) = (rx * a.cos(), ry * a.sin());
        out.push([cos * x - sin * y + cx, sin * x + cos * y + cy]);
    }
}

// ---------------------------------------------------------------- the moods' loops

/// The cat has two moods and nothing else. At work it types with its far paw, works the mouse with the near one and keeps
/// its eyes on the screen; slacking, it leaves both paws where they are, looks away and gets drowsy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CatMood {
    Work,
    Slack,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CatTransform {
    ScaleY(f32),
    Rotate(f32),
    TranslateX(f32),
    TranslateY(f32),
    Translate(f32, f32),
}

impl CatTransform {
    pub fn at_rest(self) -> bool {
        match self {
            Self::ScaleY(v) => v == 1.0,
            Self::Rotate(v) | Self::TranslateX(v) | Self::TranslateY(v) => v == 0.0,
            Self::Translate(x, y) => x == 0.0 && y == 0.0,
        }
    }

    fn lerp(self, to: Self, t: f32) -> Self {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        match (self, to) {
            (Self::ScaleY(a), Self::ScaleY(b)) => Self::ScaleY(mix(a, b)),
            (Self::Rotate(a), Self::Rotate(b)) => Self::Rotate(mix(a, b)),
            (Self::TranslateX(a), Self::TranslateX(b)) => Self::TranslateX(mix(a, b)),
            (Self::TranslateY(a), Self::TranslateY(b)) => Self::TranslateY(mix(a, b)),
            (Self::Translate(ax, ay), Self::Translate(bx, by)) => Self::Translate(mix(ax, bx), mix(ay, by)),
            _ => self,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CatPart {
    Figure,
    Tail,
    FarPaw,
    NearPaw,
    Pupils,
    Blink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopEasing {
    EaseInOut,
    Linear,
}

/// One `animation:` of a mood.
#[derive(Clone, Copy, Debug)]
pub struct CatLoop {
    pub part: CatPart,
    pub name: &'static str,
    pub duration_ms: u64,
    pub easing: LoopEasing,
    pub stops: &'static [(f32, CatTransform)],
}

use CatTransform::{Rotate, ScaleY, Translate, TranslateX, TranslateY};

const BREATHE: &[(f32, CatTransform)] = &[(0.0, ScaleY(1.0)), (50.0, ScaleY(1.015)), (100.0, ScaleY(1.0))];
const BREATHE_DEEP: &[(f32, CatTransform)] = &[(0.0, ScaleY(1.0)), (50.0, ScaleY(1.035)), (100.0, ScaleY(1.0))];
const WAG: &[(f32, CatTransform)] = &[(0.0, Rotate(0.0)), (30.0, Rotate(5.0)), (70.0, Rotate(-3.0)), (100.0, Rotate(0.0))];
const SWISH: &[(f32, CatTransform)] = &[(0.0, Rotate(0.0)), (35.0, Rotate(8.0)), (75.0, Rotate(-6.0)), (100.0, Rotate(0.0))];
/// Five keystrokes in two bursts, each a quick lift of the paw off the keys about the wrist, then it rests while the other
/// paw works the mouse.
const TYPE: &[(f32, CatTransform)] = &[
    (0.0, Rotate(0.0)),
    (3.0, Rotate(-11.0)),
    (6.0, Rotate(0.0)),
    (9.0, Rotate(-8.0)),
    (12.0, Rotate(0.0)),
    (15.0, Rotate(-12.0)),
    (18.0, Rotate(0.0)),
    (26.0, Rotate(0.0)),
    (29.0, Rotate(-10.0)),
    (32.0, Rotate(0.0)),
    (35.0, Rotate(-12.0)),
    (38.0, Rotate(0.0)),
    (100.0, Rotate(0.0)),
];
/// The near paw slides along the desk and back (`CAT_NEAR_PAW_OVERLAP` has to cover the far end of its reach) and presses
/// down once at the end for the click.
const MOUSE: &[(f32, CatTransform)] = &[
    (0.0, Translate(0.0, 0.0)),
    (40.0, Translate(0.0, 0.0)),
    (50.0, Translate(-9.0, 0.0)),
    (57.0, Translate(-9.0, 0.0)),
    (68.0, Translate(7.0, 0.0)),
    (75.0, Translate(7.0, 0.0)),
    (78.0, Translate(7.0, 3.0)),
    (81.0, Translate(7.0, 0.0)),
    (93.0, Translate(0.0, 0.0)),
    (100.0, Translate(0.0, 0.0)),
];
/// Reading along the line being typed, then following the pointer.
const FOLLOW: &[(f32, CatTransform)] = &[
    (0.0, TranslateX(0.0)),
    (36.0, TranslateX(7.0)),
    (42.0, TranslateX(7.0)),
    (50.0, TranslateX(-5.0)),
    (57.0, TranslateX(-5.0)),
    (68.0, TranslateX(6.0)),
    (82.0, TranslateX(6.0)),
    (93.0, TranslateX(0.0)),
    (100.0, TranslateX(0.0)),
];
/// A lid lowered 62 closes the eye with 8 to spare, which keeps a closed eye from leaving a ghost of its outline.
const BLINK: &[(f32, CatTransform)] = &[(0.0, TranslateY(0.0)), (18.0, TranslateY(0.0)), (21.0, TranslateY(62.0)), (24.0, TranslateY(0.0)), (100.0, TranslateY(0.0))];
/// Slacking lids already hang 24, so they need 38.
const DOZE: &[(f32, CatTransform)] = &[(0.0, TranslateY(0.0)), (74.0, TranslateY(0.0)), (82.0, TranslateY(38.0)), (87.0, TranslateY(38.0)), (95.0, TranslateY(0.0)), (100.0, TranslateY(0.0))];
const GLANCE_BACK: &[(f32, CatTransform)] = &[(0.0, TranslateX(0.0)), (30.0, TranslateX(0.0)), (40.0, TranslateX(22.0)), (58.0, TranslateX(22.0)), (72.0, TranslateX(0.0)), (100.0, TranslateX(0.0))];

const fn looped(part: CatPart, name: &'static str, duration_ms: u64, easing: LoopEasing, stops: &'static [(f32, CatTransform)]) -> CatLoop {
    CatLoop { part, name, duration_ms, easing, stops }
}

const WORK_LOOPS: &[CatLoop] = &[
    looped(CatPart::Figure, "stream-cat-breathe", 2400, LoopEasing::EaseInOut, BREATHE),
    looped(CatPart::Tail, "stream-cat-wag", 2400, LoopEasing::EaseInOut, WAG),
    looped(CatPart::FarPaw, "stream-cat-type", 2400, LoopEasing::EaseInOut, TYPE),
    looped(CatPart::NearPaw, "stream-cat-mouse", 2400, LoopEasing::EaseInOut, MOUSE),
    looped(CatPart::Pupils, "stream-cat-follow", 2400, LoopEasing::EaseInOut, FOLLOW),
    looped(CatPart::Blink, "stream-cat-blink", 4800, LoopEasing::Linear, BLINK),
];
const SLACK_LOOPS: &[CatLoop] = &[
    looped(CatPart::Figure, "stream-cat-breathe-deep", 4800, LoopEasing::EaseInOut, BREATHE_DEEP),
    looped(CatPart::Tail, "stream-cat-swish", 4800, LoopEasing::EaseInOut, SWISH),
    looped(CatPart::Pupils, "stream-cat-glance-back", 4800, LoopEasing::EaseInOut, GLANCE_BACK),
    looped(CatPart::Blink, "stream-cat-doze", 4800, LoopEasing::EaseInOut, DOZE),
];

pub fn mood_loops(mood: CatMood) -> &'static [CatLoop] {
    match mood {
        CatMood::Work => WORK_LOOPS,
        CatMood::Slack => SLACK_LOOPS,
    }
}

/// `.stream-waiting__cat--<mood> .stream-cat-gaze` and `.stream-cat-lids`: the pose, which eases across a change of mood.
pub fn mood_pose(mood: CatMood) -> (f32, f32) {
    match mood {
        CatMood::Work => (13.0, 0.0),
        CatMood::Slack => (-16.0, 24.0),
    }
}

/// CSS's `cubic-bezier`, solved for `x`.
pub(crate) fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    // Exactly: a loop's first and last stops are its resting pose, and bisection only comes close.
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let curve = |a: f32, b: f32, t: f32| {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    };
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..20 {
        let middle = (low + high) * 0.5;
        if curve(x1, x2, middle) < x {
            low = middle;
        } else {
            high = middle;
        }
    }
    curve(y1, y2, (low + high) * 0.5)
}

pub(crate) fn ease_in_out(x: f32) -> f32 {
    cubic_bezier(0.42, 0.0, 0.58, 1.0, x)
}

/// A loop at `elapsed_ms`. CSS applies the timing function to each stretch between stops, not to the whole cycle.
pub fn sample_loop(animation: &CatLoop, elapsed_ms: f64) -> CatTransform {
    let duration = animation.duration_ms as f64;
    let percent = ((elapsed_ms.rem_euclid(duration)) / duration * 100.0) as f32;
    let stops = animation.stops;
    let next = stops.iter().position(|(stop, _)| *stop > percent).unwrap_or(stops.len() - 1).max(1);
    let (from_stop, from) = stops[next - 1];
    let (to_stop, to) = stops[next];
    let local = ((percent - from_stop) / (to_stop - from_stop)).clamp(0.0, 1.0);
    from.lerp(to, if animation.easing == LoopEasing::Linear { local } else { ease_in_out(local) })
}

/// Everything that moves, for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CatPose {
    pub figure_scale_y: f32,
    pub tail_deg: f32,
    pub far_paw_deg: f32,
    pub near_paw: (f32, f32),
    /// The gaze's pose plus the pupils' own loop.
    pub pupils_x: f32,
    /// The lids' pose plus the blink.
    pub lids_y: f32,
}

impl CatPose {
    /// A mood's loops at `elapsed_ms`, over `gaze` and `lids` as the change of mood has eased them so far.
    pub fn at(mood: CatMood, elapsed_ms: f64, gaze: f32, lids: f32) -> Self {
        let mut pose = Self { figure_scale_y: 1.0, tail_deg: 0.0, far_paw_deg: 0.0, near_paw: (0.0, 0.0), pupils_x: gaze, lids_y: lids };
        for animation in mood_loops(mood) {
            match (animation.part, sample_loop(animation, elapsed_ms)) {
                (CatPart::Figure, ScaleY(v)) => pose.figure_scale_y = v,
                (CatPart::Tail, Rotate(v)) => pose.tail_deg = v,
                (CatPart::FarPaw, Rotate(v)) => pose.far_paw_deg = v,
                (CatPart::NearPaw, Translate(x, y)) => pose.near_paw = (x, y),
                (CatPart::Pupils, TranslateX(v)) => pose.pupils_x += v,
                (CatPart::Blink, TranslateY(v)) => pose.lids_y += v,
                _ => {}
            }
        }
        pose
    }
}

// ---------------------------------------------------------------- scan conversion

#[derive(Clone, Copy, PartialEq, Eq)]
enum FillRule {
    NonZero,
    EvenOdd,
}

struct CatDrawing {
    tail: Vec<Polyline>,
    laptop: Vec<Polyline>,
    /// With the face punched out of it, which is the one shape that fills by even-odd.
    body: Vec<Polyline>,
    far_paw: Vec<Polyline>,
    near_paw: Vec<Polyline>,
    pupils: Vec<Polyline>,
    lids: Vec<Polyline>,
}

static DRAWING: LazyLock<CatDrawing> = LazyLock::new(|| {
    let body = format!("{}{CAT_EYE_WHITES}{CAT_WHISKERS}{CAT_MUZZLE}", CAT_CUTS.body_without_paws);
    CatDrawing {
        tail: flatten(CAT_TAIL),
        laptop: flatten(CAT_LAPTOP),
        body: flatten(&body),
        far_paw: flatten(&CAT_CUTS.far_paw),
        near_paw: flatten(&CAT_CUTS.near_paw),
        pupils: flatten(CAT_PUPILS),
        lids: flatten(CAT_LIDS),
    }
});

/// A shape as this frame placed it: its edges in device pixels, each upper end first, with the way it winds. Horizontal edges cross nothing.
struct ScanShape {
    rule: FillRule,
    edges: Vec<([f32; 2], [f32; 2], i32)>,
}

fn scan_shape(shape: &[Polyline], rule: FillRule, map: impl Fn([f32; 2]) -> [f32; 2]) -> ScanShape {
    let mut edges = Vec::with_capacity(shape.iter().map(Vec::len).sum());
    for line in shape {
        let mut previous = map(line[line.len() - 1]);
        for &point in line {
            let current = map(point);
            if previous[1] < current[1] {
                edges.push((previous, current, 1));
            } else if current[1] < previous[1] {
                edges.push((current, previous, -1));
            }
            previous = current;
        }
    }
    ScanShape { rule, edges }
}

/// One sub-row's spans of a shape, in the rule it fills by.
fn shape_spans(shape: &ScanShape, y: f32, crossings: &mut Vec<(f32, i32)>, spans: &mut Vec<(f32, f32)>) {
    crossings.clear();
    for (upper, lower, direction) in &shape.edges {
        if upper[1] <= y && y < lower[1] {
            let t = (y - upper[1]) / (lower[1] - upper[1]);
            crossings.push((upper[0] + (lower[0] - upper[0]) * t, *direction));
        }
    }
    crossings.sort_by(|a, b| a.0.total_cmp(&b.0));
    let inside = |winding: i32| if shape.rule == FillRule::EvenOdd { winding % 2 != 0 } else { winding != 0 };
    let mut winding = 0;
    let mut start = 0.0;
    for &(x, direction) in crossings.iter() {
        let was_inside = inside(winding);
        winding += direction;
        let is_inside = inside(winding);
        if !was_inside && is_inside {
            start = x;
        } else if was_inside && !is_inside {
            spans.push((start, x));
        }
    }
}

const SUB_ROWS: usize = 4;

/// The cat's coverage for one frame, `width * height` values in 0.0..=1.0, row by row. The ledge is not in it: it has a
/// colour of its own, and the cat is drawn over it.
pub fn rasterize_cat(pose: &CatPose, width: usize, height: usize) -> Vec<f32> {
    let (vx, vy, _, vh) = CAT_SCENE_VIEWBOX;
    let k = height as f32 / vh;
    let drawing = &*DRAWING;
    let scene = |[x, y]: [f32; 2]| [(x - vx) * k, (y - vy) * k];
    // The figure's breath is outermost, then what lowers it onto the ledge, then the part's own move.
    let figure = |[x, y]: [f32; 2]| [x, FIGURE_ORIGIN_Y + (y + CAT_ON_LEDGE_Y - FIGURE_ORIGIN_Y) * pose.figure_scale_y];
    let rotated = |origin: (f32, f32), deg: f32| {
        let (sin, cos) = deg.to_radians().sin_cos();
        move |[x, y]: [f32; 2]| {
            let (dx, dy) = (x - origin.0, y - origin.1);
            [origin.0 + dx * cos - dy * sin, origin.1 + dx * sin + dy * cos]
        }
    };
    let tail_turn = rotated(TAIL_ORIGIN, pose.tail_deg);
    let paw_turn = rotated(CAT_WRIST, pose.far_paw_deg);

    // Placed once a frame, since every sub-row reads them.
    let shapes = [
        scan_shape(&drawing.tail, FillRule::NonZero, |p| scene(tail_turn(p))),
        scan_shape(&drawing.laptop, FillRule::NonZero, |[x, y]| scene([x, y + CAT_ON_LEDGE_Y])),
        scan_shape(&drawing.body, FillRule::EvenOdd, |p| scene(figure(p))),
        scan_shape(&drawing.far_paw, FillRule::NonZero, |p| scene(figure(paw_turn(p)))),
        scan_shape(&drawing.near_paw, FillRule::NonZero, |[x, y]| scene(figure([x + pose.near_paw.0, y + pose.near_paw.1]))),
        scan_shape(&drawing.pupils, FillRule::NonZero, |[x, y]| scene(figure([x + pose.pupils_x, y]))),
        scan_shape(&drawing.lids, FillRule::NonZero, |[x, y]| scene(figure([x, y + pose.lids_y]))),
    ];

    let mut out = vec![0.0f32; width * height];
    let mut crossings = Vec::new();
    let mut spans: Vec<(f32, f32)> = Vec::new();
    for row in 0..height {
        for sub in 0..SUB_ROWS {
            let y = row as f32 + (sub as f32 + 0.5) / SUB_ROWS as f32;
            spans.clear();
            for shape in &shapes {
                shape_spans(shape, y, &mut crossings, &mut spans);
            }
            spans.sort_by(|a, b| a.0.total_cmp(&b.0));
            // One colour over itself is invisible, so overlapping shapes count once.
            let mut merged_end = f32::NEG_INFINITY;
            for &(from, to) in spans.iter() {
                let from = from.max(merged_end).max(0.0);
                let to = to.min(width as f32);
                if to <= from {
                    continue;
                }
                merged_end = merged_end.max(to);
                let first = from as usize;
                let last = (to.ceil() as usize).min(width);
                for column in first..last {
                    let overlap = (to.min(column as f32 + 1.0) - from.max(column as f32)).max(0.0);
                    out[row * width + column] += overlap / SUB_ROWS as f32;
                }
            }
        }
    }
    out
}

/// The logical size the stylesheet gives the cat for a line this tall.
pub fn cat_size(line: f32) -> (f32, f32) {
    (line * CAT_SCENE_VIEWBOX.2 / CAT_SCENE_VIEWBOX.3, line)
}

/// Paints one frame of the cat in `r`.
pub fn paint_cat(ui: &mut Ui<'_>, r: Rect, pose: &CatPose, color: Color, ledge: Color) {
    let scale = ui.scale.max(0.1);
    let (vx, vy, _, vh) = CAT_SCENE_VIEWBOX;
    let k = r.h / vh;
    ui.fill(rect(r.x + (LEDGE.0 - vx) * k, r.y + (LEDGE.1 - vy) * k, LEDGE.2 * k, (LEDGE.3 * k).max(1.0 / scale)), LEDGE_RADIUS * k, ledge);

    // Laid out on device pixels: `ui.fill` rounds there anyway, and a run that was not on them would lose its neighbours.
    let (ox, oy) = ((r.x * scale).round(), (r.y * scale).round());
    let (width, height) = ((r.w * scale).round() as usize, (r.h * scale).round() as usize);
    if width == 0 || height == 0 {
        return;
    }
    let coverage = rasterize_cat(pose, width, height);
    let level = |c: f32| (c.clamp(0.0, 1.0) * 16.0).round() as u8;
    for row in 0..height {
        let line = &coverage[row * width..(row + 1) * width];
        let mut column = 0;
        while column < width {
            let shade = level(line[column]);
            let run = line[column..].iter().take_while(|c| level(**c) == shade).count();
            if shade > 0 {
                ui.fill(rect((ox + column as f32) / scale, (oy + row as f32) / scale, run as f32 / scale, 1.0 / scale), 0.0, color.fade(shade as f32 / 16.0));
            }
            column += run;
        }
    }
}
