//! The robot cell: where things are, and how the arm moves between them.
//!
//! Units are metres and radians. The world frame has x to the right, y forward
//! (away from the robot base) and z up. The table top is z = 0 and the robot
//! base is at the origin.
//!
//! The arm has four joints: base yaw `q0`, shoulder pitch `q1` (upper arm angle
//! from horizontal), elbow `q2` (relative; negative bends down) and wrist `q3`
//! (relative; kept so the tool points straight down). It also has a gripper
//! (0 closed … 1 open). Motions are joint-space moves with speed limits. Travel
//! happens at a safe height that is checked along the path: no dips into walls
//! or parts, and no reaching back over the base. Parts sit in slots inside their
//! containers, which lie on arcs around the arm.

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

pub const SHOULDER_H: f64 = 0.30;
pub const L1: f64 = 0.30;
pub const L2: f64 = 0.28;
/// Wrist pivot to fingertips (the tool points down).
pub const L3: f64 = 0.10;
/// Travel height of the fingertips.
pub const SAFE_Z: f64 = 0.19;
/// Higher route when a direct move at the safe height would dip.
pub const HIGH_Z: f64 = 0.28;
/// While travelling, the fingertips stay above this (bin walls 0.07 + a carried part).
pub const CLEAR_Z: f64 = 0.115;
/// The fingertips never come closer to the base axis than this.
pub const MIN_R: f64 = 0.12;

/// Joint speed limits at 1x (rad/s): base, shoulder, elbow, wrist.
pub const JOINT_SPEED: [f64; 4] = [2.0, 1.6, 2.0, 3.0];
pub const JOINT_MIN: [f64; 4] = [-PI, -0.35, -2.8, -3.6];
pub const JOINT_MAX: [f64; 4] = [PI, 2.6, 0.0, 1.3];
pub const GRIP_MS: i64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Arm {
    pub q: [f64; 4],
    /// 0 closed … 1 open
    pub grip: f64,
}

/// The arm's resting pose: folded, looking forward, gripper open.
pub fn home() -> Arm {
    let q = ik(0.0, 0.36, SAFE_Z + 0.04).expect("home is reachable");
    Arm { q, grip: 1.0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct P3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// Fingertip position for joint angles.
pub fn fk(q: &[f64; 4]) -> P3 {
    let (a1, a2, a3) = (q[1], q[1] + q[2], q[1] + q[2] + q[3]);
    let r = L1 * a1.cos() + L2 * a2.cos() + L3 * a3.cos();
    let z = SHOULDER_H + L1 * a1.sin() + L2 * a2.sin() + L3 * a3.sin();
    P3 {
        x: r * q[0].cos(),
        y: r * q[0].sin(),
        z,
    }
}

/// Shoulder, elbow, wrist and fingertip positions for joint angles (the links
/// run between them; the physics checks them for collisions).
pub fn joints(q: &[f64; 4]) -> [P3; 4] {
    let (a1, a2, a3) = (q[1], q[1] + q[2], q[1] + q[2] + q[3]);
    let (re, ze) = (L1 * a1.cos(), SHOULDER_H + L1 * a1.sin());
    let (rw, zw) = (re + L2 * a2.cos(), ze + L2 * a2.sin());
    let (rt, zt) = (rw + L3 * a3.cos(), zw + L3 * a3.sin());
    let at = |r: f64, z: f64| P3 {
        x: r * q[0].cos(),
        y: r * q[0].sin(),
        z,
    };
    [at(0.0, SHOULDER_H), at(re, ze), at(rw, zw), at(rt, zt)]
}

/// Joint angles that put the fingertips at (x, y, z) with the tool pointing
/// straight down (elbow up), if reachable within the joint limits.
pub fn ik(x: f64, y: f64, z: f64) -> Option<[f64; 4]> {
    let r = x.hypot(y);
    if r < MIN_R * 0.9 {
        return None;
    }
    let q0 = y.atan2(x);
    let (dr, dz) = (r, z + L3 - SHOULDER_H);
    let d2 = dr * dr + dz * dz;
    let c2 = (d2 - L1 * L1 - L2 * L2) / (2.0 * L1 * L2);
    if !(-1.0..=1.0).contains(&c2) {
        return None;
    }
    let q2 = -c2.acos();
    let q1 = dz.atan2(dr) + (L2 * (-q2).sin()).atan2(L1 + L2 * q2.cos());
    let q3 = -PI / 2.0 - (q1 + q2);
    let q = [q0, q1, q2, q3];
    within_limits(&q).then_some(q)
}

pub fn within_limits(q: &[f64; 4]) -> bool {
    (0..4).all(|i| q[i] >= JOINT_MIN[i] - 1e-9 && q[i] <= JOINT_MAX[i] + 1e-9)
}

/// One commanded motion segment. `carrying` names the part in the gripper.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Segment {
    pub kind: &'static str,
    pub from: Arm,
    pub to: Arm,
    /// Duration at 1x speed.
    pub nominal_ms: i64,
    pub carrying: Option<String>,
}

/// Vertical moves (up or down) go in a straight line, like a robot's linear
/// move, so a part lowered into a bin does not swing into its wall; the other
/// moves are joint moves.
pub fn is_straight(kind: &str) -> bool {
    matches!(kind, "descend" | "lift" | "rise" | "lower")
}

/// The joint angles a fraction `u` (0..1) of the way along a segment.
pub fn along(s: &Segment, u: f64) -> [f64; 4] {
    let joint = || std::array::from_fn(|j| s.from.q[j] + (s.to.q[j] - s.from.q[j]) * u);
    if !is_straight(s.kind) || s.from.q == s.to.q {
        return joint();
    }
    let (a, b) = (fk(&s.from.q), fk(&s.to.q));
    ik(
        a.x + (b.x - a.x) * u,
        a.y + (b.y - a.y) * u,
        a.z + (b.z - a.z) * u,
    )
    .unwrap_or_else(joint)
}

fn joint_ms(a: &[f64; 4], b: &[f64; 4]) -> i64 {
    let t = (0..4)
        .map(|i| (b[i] - a[i]).abs() / JOINT_SPEED[i])
        .fold(0.0, f64::max);
    // Smooth start and stop take longer than a constant-speed move.
    ((t * 1.4 + 0.12) * 1000.0).round() as i64
}

fn seg(kind: &'static str, from: Arm, to: Arm, carrying: Option<&str>) -> Segment {
    let ms = if from.q == to.q {
        GRIP_MS
    } else {
        joint_ms(&from.q, &to.q)
    };
    Segment {
        kind,
        from,
        to,
        nominal_ms: ms,
        carrying: carrying.map(String::from),
    }
}

/// Along a joint-space move, the lowest fingertip height and the closest
/// approach to the base axis.
fn path_clearance(a: &[f64; 4], b: &[f64; 4]) -> (f64, f64) {
    let mut lowest = f64::MAX;
    let mut closest = f64::MAX;
    for i in 0..=24 {
        let t = i as f64 / 24.0;
        let q: [f64; 4] = std::array::from_fn(|j| a[j] + (b[j] - a[j]) * t);
        let p = fk(&q);
        lowest = lowest.min(p.z);
        closest = closest.min(p.x.hypot(p.y));
    }
    (lowest, closest)
}

/// Plan from the current arm pose to the fingertip position `(x, y, z)`: up to
/// the safe height, across (by a higher route if the direct one would dip), then
/// down. Returns an explanation if any part of it is out of reach or unsafe.
pub fn plan_to(
    from: Arm,
    x: f64,
    y: f64,
    z: f64,
    carrying: Option<&str>,
) -> Result<Vec<Segment>, String> {
    plan_to_clear(from, x, y, z, carrying, &|_| None)
}

/// Cruise heights for going over something in the way.
const OVER: [f64; 3] = [HIGH_Z, 0.36, 0.44];

/// As `plan_to`, around solid things: `touches` names what a segment would hit
/// (the physics checks it). The usual route is tried first, then routes over at
/// increasing heights. With nothing in the way the result is exactly `plan_to`.
/// If every route is blocked, nothing moves and the reason names what is in the way.
pub fn plan_to_clear(
    from: Arm,
    x: f64,
    y: f64,
    z: f64,
    carrying: Option<&str>,
    touches: &dyn Fn(&Segment) -> Option<String>,
) -> Result<Vec<Segment>, String> {
    let usual = route(from, x, y, z, carrying, None)?;
    let Some(hit) = usual.iter().find_map(touches) else {
        return Ok(usual);
    };
    for h in OVER {
        if let Ok(r) = route(from, x, y, z, carrying, Some(h))
            && r.iter().find_map(touches).is_none()
        {
            return Ok(r);
        }
    }
    Err(format!("no clear path: the arm would hit {hit}"))
}

/// One route: the usual one (`over` = None), or up and over at height `over`.
fn route(
    from: Arm,
    x: f64,
    y: f64,
    z: f64,
    carrying: Option<&str>,
    over: Option<f64>,
) -> Result<Vec<Segment>, String> {
    let mut out = vec![];
    let mut cur = from;
    let here = fk(&cur.q);
    let target_down = ik(x, y, z)
        .ok_or_else(|| format!("({x:.2}, {y:.2}) at height {z:.2} is out of the arm's reach"))?;
    let above =
        ik(x, y, SAFE_Z).ok_or_else(|| format!("({x:.2}, {y:.2}) is out of the arm's reach"))?;
    // Up first, straight above where the fingertips are now.
    if here.z < SAFE_Z - 0.005 {
        let up = ik(here.x, here.y, SAFE_Z).ok_or("cannot lift straight up from here")?;
        let next = Arm {
            q: up,
            grip: cur.grip,
        };
        out.push(seg("lift", cur, next, carrying));
        cur = next;
    }
    if let Some(h) = over {
        let p = fk(&cur.q);
        let up = ik(p.x, p.y, h).ok_or("cannot rise that high here")?;
        let over_q = ik(x, y, h).ok_or("cannot reach over at that height")?;
        let (low, near) = path_clearance(&up, &over_q);
        if low < CLEAR_Z || near < MIN_R {
            return Err("no safe path over: the arm would pass too close to its base".into());
        }
        for (kind, q) in [("rise", up), ("travel", over_q), ("lower", above)] {
            if q != cur.q {
                let next = Arm { q, grip: cur.grip };
                out.push(seg(kind, cur, next, carrying));
                cur = next;
            }
        }
    } else if cur.q != above {
        // Across at the safe height, or by the higher route.
        let (low, near) = path_clearance(&cur.q, &above);
        if low >= CLEAR_Z && near >= MIN_R {
            let next = Arm {
                q: above,
                grip: cur.grip,
            };
            out.push(seg("travel", cur, next, carrying));
            cur = next;
        } else {
            let p = fk(&cur.q);
            let up = ik(p.x, p.y, HIGH_Z).ok_or("cannot rise to the high route")?;
            let over = ik(x, y, HIGH_Z).ok_or("cannot reach over at the high route")?;
            let (low2, near2) = path_clearance(&up, &over);
            if low2 < CLEAR_Z || near2 < MIN_R {
                return Err(
                    "no safe path: the arm would dip into the cell or pass too close to its base"
                        .into(),
                );
            }
            for (kind, q) in [("rise", up), ("travel", over), ("lower", above)] {
                let next = Arm { q, grip: cur.grip };
                out.push(seg(kind, cur, next, carrying));
                cur = next;
            }
        }
    }
    let next = Arm {
        q: target_down,
        grip: cur.grip,
    };
    out.push(seg("descend", cur, next, carrying));
    Ok(out)
}

/// Back up to the safe height straight above the fingertips.
pub fn plan_lift(from: Arm, carrying: Option<&str>) -> Vec<Segment> {
    let p = fk(&from.q);
    if p.z >= SAFE_Z - 0.005 {
        return vec![];
    }
    match ik(p.x, p.y, SAFE_Z) {
        Some(q) => vec![seg("lift", from, Arm { q, grip: from.grip }, carrying)],
        None => vec![],
    }
}

pub fn plan_grip(from: Arm, grip: f64, carrying: Option<&str>) -> Segment {
    seg(
        if grip < from.grip { "close" } else { "open" },
        from,
        Arm { q: from.q, grip },
        carrying,
    )
}

// ------------------------------------------------------------------ layout

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Place {
    pub id: String,
    /// bin | tray | table
    pub kind: String,
    pub cx: f64,
    pub cy: f64,
    /// Rotation about z (radians): the place faces the arm.
    pub yaw: f64,
    /// Size across (tangential) and deep (radial); for the table pile, the radius.
    pub w: f64,
    pub d: f64,
    pub wall: f64,
    pub floor: f64,
}

fn polar(id: &str, kind: &str, r: f64, deg: f64, w: f64, d: f64, wall: f64) -> Place {
    let a = deg.to_radians();
    Place {
        id: id.into(),
        kind: kind.into(),
        cx: r * a.cos(),
        cy: r * a.sin(),
        yaw: a - PI / 2.0,
        w,
        d,
        wall,
        floor: if wall > 0.0 { 0.004 } else { 0.0 },
    }
}

/// Where each place is. Bins on the left arc, trays 1–4 on the right arc, trays 5
/// and 6 on an inner arc either side of the table pile, which is in front of the arm.
pub fn place_for(id: &str, loc_type: &str) -> Option<Place> {
    const BIN: (f64, f64, f64) = (0.13, 0.13, 0.07);
    const TRAY: (f64, f64, f64) = (0.13, 0.12, 0.025);
    match loc_type {
        "bin" => {
            let deg = match id {
                "bin-red" => 168.0,
                "bin-blue" => 146.0,
                "bin-green" => 124.0,
                "bin-washer" => 102.0,
                _ => 180.0,
            };
            Some(polar(id, "bin", 0.46, deg, BIN.0, BIN.1, BIN.2))
        }
        "tray" => {
            let n: usize = id.strip_prefix("tray-")?.parse().ok()?;
            let (r, deg) = match n {
                1 => (0.46, 12.0),
                2 => (0.46, 34.0),
                3 => (0.46, 56.0),
                4 => (0.46, 78.0),
                5 => (0.25, 38.0),
                6 => (0.25, 142.0),
                // The cell has room for six trays; more have no place to be.
                _ => return None,
            };
            Some(polar(id, "tray", r, deg, TRAY.0, TRAY.1, TRAY.2))
        }
        "table" => Some(polar(id, "table", 0.25, 90.0, 0.10, 0.10, 0.0)),
        _ => None,
    }
}

/// Height of a part of this kind (centre to top is half of it).
pub fn part_height(kind: &str) -> f64 {
    match kind {
        "washer" => 0.008,
        "red_block" | "blue_block" | "green_block" => 0.032,
        _ => 0.03,
    }
}

const LAYER: f64 = 0.034;

/// Slots per layer in a place.
pub fn slots_per_layer(p: &Place) -> usize {
    match p.kind.as_str() {
        "bin" => 9,
        "tray" => 6,
        _ => 12,
    }
}

/// How a part in slot `i` is turned about the vertical: square to its bin or
/// tray (which face the arm), or at a varied angle in the loose pile.
pub fn slot_yaw(p: &Place, i: usize) -> f64 {
    match p.kind.as_str() {
        "bin" | "tray" => p.yaw,
        // Turned a little either way, so neighbours never overlap.
        _ => (i as f64 * 0.7) % 0.7 - 0.35,
    }
}

/// Centre of slot `i` for a part of height `h` (layers stack upwards).
pub fn slot_position(p: &Place, i: usize, h: f64) -> P3 {
    let per = slots_per_layer(p);
    let (layer, k) = (i / per, i % per);
    let (u, v) = match p.kind.as_str() {
        "bin" => ((k % 3) as f64 * 0.04 - 0.04, (k / 3) as f64 * 0.04 - 0.04),
        "tray" => ((k % 3) as f64 * 0.04 - 0.04, (k / 3) as f64 * 0.05 - 0.025),
        _ => {
            // A loose, deterministic pile (sunflower pattern).
            let r = 0.085 * ((k as f64 + 0.5) / per as f64).sqrt();
            let a = k as f64 * 2.399_963;
            (r * a.cos(), r * a.sin())
        }
    };
    let (s, c) = p.yaw.sin_cos();
    P3 {
        x: p.cx + u * c - v * s,
        y: p.cy + u * s + v * c,
        z: p.floor + layer as f64 * LAYER + h / 2.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> Vec<Place> {
        let mut v: Vec<Place> = ["bin-red", "bin-blue", "bin-green", "bin-washer"]
            .iter()
            .filter_map(|b| place_for(b, "bin"))
            .collect();
        v.extend((1..=6).filter_map(|t| place_for(&format!("tray-{t}"), "tray")));
        v.push(place_for("table", "table").unwrap());
        v
    }

    #[test]
    fn inverse_kinematics_round_trips() {
        for p in places() {
            for i in 0..slots_per_layer(&p) * 2 {
                let s = slot_position(&p, i, 0.032);
                let top = s.z + 0.016;
                let q =
                    ik(s.x, s.y, top).unwrap_or_else(|| panic!("{} slot {i} unreachable", p.id));
                let back = fk(&q);
                assert!(
                    (back.x - s.x).abs() < 1e-9
                        && (back.y - s.y).abs() < 1e-9
                        && (back.z - top).abs() < 1e-9
                );
                assert!(
                    (q[1] + q[2] + q[3] + PI / 2.0).abs() < 1e-9,
                    "tool points down"
                );
                assert!(
                    ik(s.x, s.y, SAFE_Z).is_some() && ik(s.x, s.y, HIGH_Z).is_some(),
                    "{} slot {i} above",
                    p.id
                );
            }
        }
    }

    /// Exact overlap test for two rotated rectangles (separating axes), each grown by `m`.
    fn boxes_overlap(a: &Place, b: &Place, m: f64) -> bool {
        let corners = |p: &Place| -> Vec<(f64, f64)> {
            let (s, c) = p.yaw.sin_cos();
            let (hw, hd) = if p.kind == "table" {
                (p.w + m, p.d + m)
            } else {
                (p.w / 2.0 + m, p.d / 2.0 + m)
            };
            [(-hw, -hd), (hw, -hd), (hw, hd), (-hw, hd)]
                .iter()
                .map(|(u, v)| (p.cx + u * c - v * s, p.cy + u * s + v * c))
                .collect()
        };
        let (ca, cb) = (corners(a), corners(b));
        let axes = [a.yaw, a.yaw + PI / 2.0, b.yaw, b.yaw + PI / 2.0];
        axes.iter().all(|t| {
            let (ax, ay) = (t.cos(), t.sin());
            let proj = |pts: &Vec<(f64, f64)>| {
                pts.iter()
                    .map(|(x, y)| x * ax + y * ay)
                    .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)))
            };
            let ((a0, a1), (b0, b1)) = (proj(&ca), proj(&cb));
            a0 < b1 && b0 < a1
        })
    }

    /// The pile is a disc of radius `w`: is it within `gap` of a box?
    fn disc_near_box(disc: &Place, b: &Place, gap: f64) -> bool {
        let (s, c) = b.yaw.sin_cos();
        let (dx, dy) = (disc.cx - b.cx, disc.cy - b.cy);
        let (u, v) = (dx * c + dy * s, -dx * s + dy * c);
        let (ou, ov) = (
            (u.abs() - b.w / 2.0).max(0.0),
            (v.abs() - b.d / 2.0).max(0.0),
        );
        ou.hypot(ov) < disc.w + gap
    }

    fn overlap(a: &Place, b: &Place, m: f64) -> bool {
        match (a.kind == "table", b.kind == "table") {
            (true, false) => disc_near_box(a, b, 2.0 * m),
            (false, true) => disc_near_box(b, a, 2.0 * m),
            _ => boxes_overlap(a, b, m),
        }
    }

    #[test]
    fn places_do_not_overlap() {
        let ps = places();
        for (i, a) in ps.iter().enumerate() {
            for b in &ps[i + 1..] {
                assert!(
                    !overlap(a, b, 0.005),
                    "{} and {} are closer than 1 cm",
                    a.id,
                    b.id
                );
            }
        }
    }

    #[test]
    fn every_move_between_places_is_reachable_and_clear() {
        let ps = places();
        let mut arm = home();
        let mut longest = 0;
        for a in &ps {
            for b in &ps {
                let sa = slot_position(a, 4, 0.032);
                let sb = slot_position(b, 2, 0.032);
                let mut segs = plan_to(arm, sa.x, sa.y, sa.z + 0.016, None)
                    .unwrap_or_else(|e| panic!("to {}: {e}", a.id));
                arm = segs.last().unwrap().to;
                segs = plan_lift(arm, Some("x"));
                if let Some(s) = segs.last() {
                    arm = s.to;
                }
                let segs = plan_to(arm, sb.x, sb.y, sb.z + 0.016, Some("x"))
                    .unwrap_or_else(|e| panic!("{} -> {}: {e}", a.id, b.id));
                for s in &segs {
                    assert!(within_limits(&s.to.q));
                    if s.kind == "travel" {
                        let (low, near) = path_clearance(&s.from.q, &s.to.q);
                        assert!(low >= CLEAR_Z && near >= MIN_R, "{} -> {} dips", a.id, b.id);
                    }
                    longest = longest.max(s.nominal_ms);
                }
                arm = segs.last().unwrap().to;
            }
        }
        assert!(longest < 3000, "a single segment takes {longest} ms");
    }

    #[test]
    fn out_of_reach_is_explained() {
        assert!(
            plan_to(home(), 0.9, 0.2, 0.02, None)
                .unwrap_err()
                .contains("out of the arm's reach")
        );
        assert!(ik(0.0, 0.05, 0.1).is_none(), "too close to the base");
    }
}
