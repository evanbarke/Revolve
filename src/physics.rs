//! Rigid-body physics for the robot cell (Rapier, f64).
//!
//! Solid things: the table top, each place's floor and walls, the robot's base
//! and column, and any barriers. Parts are dynamic bodies with mass and
//! friction, under gravity. The world is rebuilt from the stored state for each
//! command, so a result depends only on that state. A part counts as moved only
//! if it moved visibly (see `moved`); resting parts keep their stored pose
//! exactly, so they do not drift from one command to the next.
//! Rapier's enhanced determinism makes it the same bit for bit on x86_64 and
//! aarch64, so Windows and the Jetson agree and a replay repeats exactly.
//!
//! Used to: let a released part fall and settle (where it comes to rest decides
//! which place it is in); settle a new table; and check that the arm, and the
//! part it carries, would not touch anything along a planned motion.
//!
//! Modelled simply, on purpose: the arm follows its commanded path exactly and
//! never pushes things (a motion that would touch something is refused before it
//! starts, like a robot's protective stop), and a grasp is an attachment made
//! when the fingers close around the part (finger friction is not simulated).

use crate::cell::{self, Place};
use rapier3d_f64::glamx::{DQuat, DVec3};
use rapier3d_f64::parry::query::ShapeCastOptions;
use rapier3d_f64::prelude::*;
use std::collections::BTreeMap;
use std::f64::consts::FRAC_PI_2;

/// Simulation step (s).
pub const DT: f64 = 1.0 / 240.0;
/// How far above whatever is below the gripper lets go of a part (m).
pub const DROP: f64 = 0.02;
/// Longest a fall is simulated for (s).
const MAX_SETTLE_S: f64 = 3.0;
/// Parts count as at rest after this long (s) below these speeds.
const STILL_S: f64 = 0.2;
const STILL_LIN: f64 = 0.004;
const STILL_ANG: f64 = 0.05;
/// Smaller changes than these are resting jitter, not movement (m, rad).
const MOVED_M: f64 = 5e-4;
const MOVED_RAD: f64 = 0.01;
/// A fall is recorded for viewers at this rate (s per frame).
const FRAME_S: f64 = 1.0 / 30.0;
/// Clearance kept between the arm and anything else (m).
const MARGIN: f64 = 0.004;
/// Clearance kept around a carried part (m).
const PART_MARGIN: f64 = 0.002;
/// The tool (wrist to fingertips): a slim rod whose open fingers straddle the
/// part being picked; radius and clearance (m).
const TOOL_R: f64 = 0.010;
const TOOL_MARGIN: f64 = 0.001;

/// Where a part is and how it is turned: centre (m) and rotation (x, y, z, w).
#[derive(Debug, Clone, PartialEq)]
pub struct PartPose {
    pub id: String,
    pub kind: String,
    pub p: [f64; 3],
    pub q: [f64; 4],
}

/// A solid barrier standing on the table: centre, size (across, deep, high) and
/// rotation about the vertical (m, rad).
#[derive(Debug, Clone, PartialEq)]
pub struct Barrier {
    pub id: String,
    pub label: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub d: f64,
    pub h: f64,
    pub yaw: f64,
}

/// A fall for viewers: frames of (seconds, x, y, z, qx, qy, qz, qw).
pub type Track = Vec<[f64; 8]>;

/// The result of letting things fall.
#[derive(Debug, Clone, Default)]
pub struct Settled {
    /// Every part whose pose changed, where it came to rest.
    pub moved: Vec<PartPose>,
    /// How each moving part got there.
    pub tracks: BTreeMap<String, Track>,
    /// Simulated time until everything was still.
    pub seconds: f64,
}

/// What a collider is: a name for people, and whether it is a part or the robot.
struct Tag {
    name: String,
    part: Option<String>,
    robot: bool,
}

pub struct CellWorld {
    w: PhysicsWorld,
    parts: BTreeMap<String, (RigidBodyHandle, String)>,
    tags: Vec<Tag>,
}

/// The collider for a part of this (true) kind, centred on its body.
fn part_collider(kind: &str) -> ColliderBuilder {
    let b = match kind {
        // A flat ring; as solid as a disc for contacts.
        "washer" => ColliderBuilder::cylinder(0.004, 0.016)
            .rotation(DVec3::new(FRAC_PI_2, 0.0, 0.0))
            .density(6000.0),
        "red_block" | "blue_block" | "green_block" => {
            ColliderBuilder::cuboid(0.016, 0.016, 0.016).density(700.0)
        }
        // Anything else (unidentified items): a rounded lump.
        _ => ColliderBuilder::round_cuboid(0.012, 0.012, 0.012, 0.003).density(900.0),
    };
    b.friction(0.7).restitution(0.05)
}

/// The shape of a part, for queries (its local frame).
fn part_shape(kind: &str) -> (SharedShape, Pose) {
    match kind {
        "washer" => (
            SharedShape::cylinder(0.004, 0.016),
            Pose::from_rotation(DQuat::from_rotation_x(FRAC_PI_2)),
        ),
        "red_block" | "blue_block" | "green_block" => {
            (SharedShape::cuboid(0.016, 0.016, 0.016), Pose::identity())
        }
        _ => (
            SharedShape::round_cuboid(0.012, 0.012, 0.012, 0.003),
            Pose::identity(),
        ),
    }
}

fn pose_of(p: &PartPose) -> Pose {
    Pose::from_parts(
        DVec3::new(p.p[0], p.p[1], p.p[2]),
        DQuat::from_xyzw(p.q[0], p.q[1], p.q[2], p.q[3]).normalize(),
    )
}

fn yaw_pose(x: f64, y: f64, z: f64, yaw: f64) -> Pose {
    Pose::from_parts(DVec3::new(x, y, z), DQuat::from_rotation_z(yaw))
}

impl CellWorld {
    /// The cell with its places, barriers and parts.
    pub fn new(places: &[Place], barriers: &[Barrier], parts: &[PartPose]) -> CellWorld {
        let mut w = PhysicsWorld::new();
        w.gravity = DVec3::new(0.0, 0.0, -9.81);
        w.integration_parameters.dt = DT;
        // Objects are a few centimetres across.
        w.integration_parameters.length_unit = 0.05;
        let mut c = CellWorld {
            w,
            parts: BTreeMap::new(),
            tags: vec![],
        };
        c.fixed(
            "the table",
            false,
            ColliderBuilder::cuboid(0.75, 0.5, 0.015),
            yaw_pose(0.0, 0.28, -0.015, 0.0),
        );
        c.fixed(
            "the robot's base",
            true,
            ColliderBuilder::cylinder(0.025, 0.085),
            Pose::from_parts(
                DVec3::new(0.0, 0.0, 0.025),
                DQuat::from_rotation_x(FRAC_PI_2),
            ),
        );
        let col = (cell::SHOULDER_H - 0.05) / 2.0;
        c.fixed(
            "the robot's column",
            true,
            ColliderBuilder::cylinder(col, 0.045),
            Pose::from_parts(
                DVec3::new(0.0, 0.0, 0.05 + col),
                DQuat::from_rotation_x(FRAC_PI_2),
            ),
        );
        for p in places.iter().filter(|p| p.kind != "table") {
            let name = place_name(p);
            let (s, co) = p.yaw.sin_cos();
            let at = |u: f64, v: f64, z: f64| {
                yaw_pose(p.cx + u * co - v * s, p.cy + u * s + v * co, z, p.yaw)
            };
            let t = 0.002;
            c.fixed(
                &format!("the floor of {name}"),
                false,
                ColliderBuilder::cuboid(p.w / 2.0, p.d / 2.0, p.floor / 2.0),
                at(0.0, 0.0, p.floor / 2.0),
            );
            let hz = p.wall / 2.0;
            for (hu, hv, u, v) in [
                (p.w / 2.0, t, 0.0, p.d / 2.0),
                (p.w / 2.0, t, 0.0, -p.d / 2.0),
                (t, p.d / 2.0, p.w / 2.0, 0.0),
                (t, p.d / 2.0, -p.w / 2.0, 0.0),
            ] {
                c.fixed(
                    &format!("the wall of {name}"),
                    false,
                    ColliderBuilder::cuboid(hu, hv, hz),
                    at(u, v, hz),
                );
            }
        }
        for b in barriers {
            c.fixed(
                &b.label,
                false,
                ColliderBuilder::cuboid(b.w / 2.0, b.d / 2.0, b.h / 2.0),
                yaw_pose(b.x, b.y, b.h / 2.0, b.yaw),
            );
        }
        for p in parts {
            c.add_part(p);
        }
        c
    }

    /// The same cell, ready for the questions below (what the arm would touch,
    /// where a part would land). Such a world is only asked, never stepped:
    /// preparing it for queries would stop Rapier from simulating its parts.
    pub fn for_queries(places: &[Place], barriers: &[Barrier], parts: &[PartPose]) -> CellWorld {
        let mut c = CellWorld::new(places, barriers, parts);
        c.w.detect_collisions(&(), &());
        c
    }

    fn tag(&mut self, t: Tag) -> u128 {
        self.tags.push(t);
        (self.tags.len() - 1) as u128
    }

    fn fixed(&mut self, name: &str, robot: bool, co: ColliderBuilder, at: Pose) {
        let i = self.tag(Tag {
            name: name.into(),
            part: None,
            robot,
        });
        self.w
            .insert_collider(co.position(at).friction(0.8).user_data(i), None);
    }

    fn add_part(&mut self, p: &PartPose) {
        let i = self.tag(Tag {
            name: p.id.clone(),
            part: Some(p.id.clone()),
            robot: false,
        });
        // Every part starts awake (Rapier 0.36 does not support bodies that
        // start asleep); resting ones simply stay put.
        let body = RigidBodyBuilder::dynamic()
            .pose(pose_of(p))
            .angular_damping(0.3);
        let (h, _) = self.w.insert(body, part_collider(&p.kind).user_data(i));
        self.parts.insert(p.id.clone(), (h, p.kind.clone()));
    }

    fn part_pose(&self, id: &str) -> Option<PartPose> {
        let (h, kind) = self.parts.get(id)?;
        let b = self.w.bodies.get(*h)?;
        let pos = b.position();
        let (t, r) = (pos.translation, pos.rotation);
        Some(PartPose {
            id: id.into(),
            kind: kind.clone(),
            p: [t.x, t.y, t.z],
            q: [r.x, r.y, r.z, r.w],
        })
    }

    /// Let go of `part` (not yet in the world) at its pose and let everything settle.
    pub fn release(&mut self, part: &PartPose) -> Settled {
        self.add_part(part);
        self.settle()
    }

    /// Step until every part is still (or the time limit), recording each moving
    /// part's fall.
    pub fn settle(&mut self) -> Settled {
        let before: BTreeMap<String, PartPose> = self
            .parts
            .keys()
            .filter_map(|id| self.part_pose(id).map(|p| (id.clone(), p)))
            .collect();
        let mut tracks: BTreeMap<String, Track> = BTreeMap::new();
        let (mut t, mut still_for, mut next_frame) = (0.0, 0.0, 0.0);
        loop {
            let moving: Vec<String> = self
                .parts
                .iter()
                .filter(|(_, (h, _))| self.w.bodies.get(*h).is_some_and(|b| !b.is_sleeping()))
                .map(|(id, _)| id.clone())
                .collect();
            if t >= next_frame - 1e-9 {
                for id in &moving {
                    if let Some(p) = self.part_pose(id) {
                        tracks.entry(id.clone()).or_default().push(frame(t, &p));
                    }
                }
                next_frame += FRAME_S;
            }
            let fast = moving.iter().any(|id| {
                let b = &self.w.bodies[self.parts[id].0];
                b.linvel().length() > STILL_LIN || b.angvel().length() > STILL_ANG
            });
            still_for = if fast { 0.0 } else { still_for + DT };
            if moving.is_empty() || still_for >= STILL_S || t >= MAX_SETTLE_S {
                break;
            }
            self.w.step();
            t += DT;
        }
        let mut moved = vec![];
        for (id, b) in &before {
            let Some(now) = self.part_pose(id) else {
                continue;
            };
            if visibly_moved(b, &now) {
                if let Some(tr) = tracks.get_mut(id) {
                    tr.push(frame(t, &now));
                }
                moved.push(now);
            }
        }
        for id in self.parts.keys() {
            if !before.contains_key(id)
                && let Some(now) = self.part_pose(id)
            {
                tracks.entry(id.clone()).or_default().push(frame(t, &now));
                moved.push(now);
            }
        }
        tracks.retain(|id, _| moved.iter().any(|m| &m.id == id));
        Settled {
            moved,
            tracks,
            seconds: t,
        }
    }

    fn tag_of(&self, co: &Collider) -> &Tag {
        &self.tags[co.user_data as usize]
    }

    /// The first thing a shape at `pos` would touch, ignoring the robot itself
    /// and the part named `ignore`.
    fn touching(&self, shape: &dyn Shape, pos: Pose, ignore: Option<&str>) -> Option<String> {
        let skip = |_h: ColliderHandle, co: &Collider| {
            let t = self.tag_of(co);
            !t.robot && (ignore.is_none() || t.part.as_deref() != ignore)
        };
        let filter = QueryFilter::default().predicate(&skip);
        let mut hits: Vec<String> = self
            .w
            .intersect_shape(pos, shape, filter)
            .map(|(_, co)| self.tag_of(co).name.clone())
            .collect();
        hits.sort();
        hits.into_iter().next()
    }

    /// What the arm, and the part of kind `carrying` in its gripper, would touch
    /// with joint angles `q`; the part named `ignore` (being picked) does not count.
    pub fn arm_touches(
        &self,
        q: &[f64; 4],
        carrying: Option<&str>,
        ignore: Option<&str>,
    ) -> Option<String> {
        let [s, e, w, t] = cell::joints(q);
        let v = |p: cell::P3| DVec3::new(p.x, p.y, p.z);
        // The tool's rounded end sits at the fingertips.
        let tool = TOOL_R + TOOL_MARGIN;
        let tool_end = DVec3::new(t.x, t.y, t.z + tool);
        let links = [
            (v(s), v(e), 0.03 + MARGIN),
            (v(e), v(w), 0.024 + MARGIN),
            (v(w), tool_end, tool),
        ];
        for (a, b, r) in links {
            let cap = Capsule::new(a, b, r);
            if let Some(hit) = self.touching(&cap, Pose::identity(), ignore) {
                return Some(hit);
            }
        }
        if let Some(kind) = carrying {
            let (shape, local) = part_shape(kind);
            let h = cell::part_height(kind);
            // The gripper holds the part turned with the arm's base.
            let at = Pose::from_parts(
                DVec3::new(t.x, t.y, t.z - h / 2.0),
                DQuat::from_rotation_z(q[0]),
            ) * local;
            // A slightly larger copy keeps the clearance.
            let grown = grow(&shape, PART_MARGIN);
            if let Some(hit) = self.touching(grown.as_ref(), at, ignore) {
                return Some(hit);
            }
        }
        None
    }

    /// What a motion segment would touch, sampled finely along its path.
    /// `carrying` is the kind of the part in the gripper during the segment. The
    /// starting pose is where the arm already is (e.g. still down in a bin with
    /// the part it just gripped, after a power cut): only where the motion goes
    /// is checked.
    pub fn segment_touches(
        &self,
        s: &cell::Segment,
        carrying: Option<&str>,
        ignore: Option<&str>,
    ) -> Option<String> {
        if s.from.q == s.to.q {
            return None;
        }
        let span = (0..4)
            .map(|i| (s.to.q[i] - s.from.q[i]).abs())
            .fold(0.0, f64::max);
        let n = ((span / 0.01).ceil() as usize).clamp(4, 400);
        (1..=n).find_map(|i| {
            let q = cell::along(s, i as f64 / n as f64);
            self.arm_touches(&q, carrying, ignore)
        })
    }

    /// Where a part of `kind` dropped straight down at (x, y) from height `z0`
    /// would first come to rest: its centre height, and what it would land on.
    pub fn rest_height(&self, kind: &str, x: f64, y: f64, z0: f64) -> Option<(f64, String)> {
        let (shape, local) = part_shape(kind);
        let at = Pose::from_translation(DVec3::new(x, y, z0)) * local;
        let skip = |_h: ColliderHandle, co: &Collider| !self.tag_of(co).robot;
        let filter = QueryFilter::default().predicate(&skip);
        let (h, hit) = self.w.cast_shape(
            &at,
            DVec3::new(0.0, 0.0, -1.0),
            shape.as_ref(),
            ShapeCastOptions::with_max_time_of_impact(z0 + 0.1),
            filter,
        )?;
        let name = self.tag_of(&self.w.colliders[h]).name.clone();
        Some((z0 - hit.time_of_impact, name))
    }

    /// The top of a part (m), as it lies.
    pub fn top_of(&self, id: &str) -> Option<f64> {
        let (h, _) = self.parts.get(id)?;
        let b = self.w.bodies.get(*h)?;
        let co = self.w.colliders.get(*b.colliders().first()?)?;
        Some(co.compute_aabb().maxs.z)
    }

    /// Another part lying on top of this one, if any.
    pub fn on_top_of(&self, id: &str) -> Option<String> {
        let (h, _) = self.parts.get(id)?;
        let b = self.w.bodies.get(*h)?;
        let co = self.w.colliders.get(*b.colliders().first()?)?;
        let bb = co.compute_aabb();
        let half = DVec3::new(
            (bb.maxs.x - bb.mins.x) * 0.4,
            (bb.maxs.y - bb.mins.y) * 0.4,
            0.012,
        );
        let c = DVec3::new(
            (bb.maxs.x + bb.mins.x) / 2.0,
            (bb.maxs.y + bb.mins.y) / 2.0,
            bb.maxs.z + 0.002 + half.z,
        );
        let probe = Cuboid::new(half);
        let only_parts = |_h: ColliderHandle, co: &Collider| {
            self.tag_of(co).part.as_deref().is_some_and(|p| p != id)
        };
        let filter = QueryFilter::default().predicate(&only_parts);
        let mut hits: Vec<String> = self
            .w
            .intersect_shape(Pose::from_translation(c), &probe, filter)
            .map(|(_, co)| self.tag_of(co).name.clone())
            .collect();
        hits.sort();
        hits.into_iter().next()
    }
}

fn grow(shape: &SharedShape, m: f64) -> SharedShape {
    if let Some(c) = shape.as_cuboid() {
        SharedShape::cuboid(
            c.half_extents.x + m,
            c.half_extents.y + m,
            c.half_extents.z + m,
        )
    } else if let Some(c) = shape.as_cylinder() {
        SharedShape::cylinder(c.half_height + m, c.radius + m)
    } else if let Some(c) = shape.as_round_cuboid() {
        let h = c.inner_shape.half_extents;
        SharedShape::round_cuboid(h.x, h.y, h.z, c.border_radius + m)
    } else {
        shape.clone()
    }
}

/// Moved by more than resting jitter.
fn visibly_moved(a: &PartPose, b: &PartPose) -> bool {
    let d = ((0..3).map(|i| (a.p[i] - b.p[i]).powi(2)).sum::<f64>()).sqrt();
    let qa = DQuat::from_xyzw(a.q[0], a.q[1], a.q[2], a.q[3]).normalize();
    let qb = DQuat::from_xyzw(b.q[0], b.q[1], b.q[2], b.q[3]).normalize();
    d > MOVED_M || qa.angle_between(qb) > MOVED_RAD
}

/// A rotation about the vertical, as stored (x, y, z, w): how a part is turned
/// in the gripper when the arm's base is at `yaw`.
pub fn turned(yaw: f64) -> [f64; 4] {
    let r = DQuat::from_rotation_z(yaw);
    [r.x, r.y, r.z, r.w]
}

fn frame(t: f64, p: &PartPose) -> [f64; 8] {
    [t, p.p[0], p.p[1], p.p[2], p.q[0], p.q[1], p.q[2], p.q[3]]
}

/// What people call a place.
pub fn place_name(p: &Place) -> String {
    match p.id.as_str() {
        "bin-red" => "the red bin".into(),
        "bin-blue" => "the blue bin".into(),
        "bin-green" => "the green bin".into(),
        "bin-washer" => "the washer bin".into(),
        id => match id.strip_prefix("tray-") {
            Some(n) => format!("tray {n}"),
            None => id.to_string(),
        },
    }
}

/// Which place a part resting with its centre at `p` is in: a bin or tray whose
/// footprint it is over (and not far above), else the table if there is one,
/// else outside the workspace.
pub fn locate(places: &[Place], has_table: bool, p: [f64; 3]) -> String {
    for pl in places.iter().filter(|pl| pl.kind != "table") {
        let (s, c) = pl.yaw.sin_cos();
        let (dx, dy) = (p[0] - pl.cx, p[1] - pl.cy);
        let (u, v) = (dx * c + dy * s, -dx * s + dy * c);
        if u.abs() <= pl.w / 2.0 && v.abs() <= pl.d / 2.0 && p[2] < pl.floor + pl.wall + 0.12 {
            return pl.id.clone();
        }
    }
    let on_table = p[0].abs() <= 0.75 && (p[1] - 0.28).abs() <= 0.5 && p[2] > -0.01;
    if on_table && has_table {
        crate::model::TABLE.into()
    } else {
        crate::sim::OUTSIDE.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> Vec<Place> {
        let mut v: Vec<Place> = ["bin-red", "bin-blue", "bin-green", "bin-washer"]
            .iter()
            .filter_map(|b| cell::place_for(b, "bin"))
            .collect();
        v.extend((1..=4).filter_map(|t| cell::place_for(&format!("tray-{t}"), "tray")));
        v.push(cell::place_for("table", "table").unwrap());
        v
    }

    fn block(id: &str, x: f64, y: f64, z: f64) -> PartPose {
        PartPose {
            id: id.into(),
            kind: "red_block".into(),
            p: [x, y, z],
            q: [0.0, 0.0, 0.0, 1.0],
        }
    }

    #[test]
    fn a_released_block_falls_into_the_bin_and_rests_on_its_floor() {
        let ps = places();
        let bin = cell::place_for("bin-red", "bin").unwrap();
        let q = CellWorld::for_queries(&ps, &[], &[]);
        let (rest, on) = q.rest_height("red_block", bin.cx, bin.cy, 0.3).unwrap();
        let mut w = CellWorld::new(&ps, &[], &[]);
        assert!((rest - (bin.floor + 0.016)).abs() < 1e-3, "{rest} on {on}");
        let s = w.release(&block("red-01", bin.cx, bin.cy, rest + DROP));
        let p = &s.moved[0];
        assert!((p.p[2] - (bin.floor + 0.016)).abs() < 2e-3, "{:?}", p.p);
        assert_eq!(locate(&ps, true, p.p), "bin-red");
        assert!(s.tracks["red-01"].len() >= 3, "the fall is recorded");
        assert!(s.seconds < 1.5, "settles quickly: {} s", s.seconds);
    }

    #[test]
    fn the_same_drop_gives_the_same_result_bit_for_bit() {
        let ps = places();
        let tray = cell::place_for("tray-2", "tray").unwrap();
        let resting = vec![block("red-02", tray.cx, tray.cy, tray.floor + 0.016)];
        let run = || {
            let mut w = CellWorld::new(&ps, &[], &resting);
            // Dropped half on top of the other block: it tips and slides.
            w.release(&block(
                "red-01",
                tray.cx + 0.02,
                tray.cy,
                tray.floor + 0.032 + 0.016 + DROP,
            ))
            .moved
        };
        let (a, b) = (run(), run());
        assert_eq!(a, b);
        assert!(
            a.iter()
                .any(|m| m.id == "red-01" && m.q != [0.0, 0.0, 0.0, 1.0]),
            "it tipped: {a:?}"
        );
    }

    #[test]
    fn the_arm_touches_a_barrier_in_its_way_and_a_covered_part_is_found() {
        let ps = places();
        let wall = Barrier {
            id: "w".into(),
            label: "the wall".into(),
            x: 0.0,
            y: 0.36,
            w: 0.3,
            d: 0.02,
            h: 0.4,
            yaw: 0.0,
        };
        let w = CellWorld::for_queries(&ps, &[wall], &[]);
        let q = cell::ik(0.0, 0.36, 0.2).unwrap();
        assert_eq!(w.arm_touches(&q, None, None).as_deref(), Some("the wall"));
        let clear = CellWorld::for_queries(&ps, &[], &[]);
        assert_eq!(clear.arm_touches(&q, Some("red_block"), None), None);
        let a = block("red-01", 0.0, 0.25, 0.016);
        let b = block("red-02", 0.0, 0.25, 0.048);
        let w = CellWorld::for_queries(&ps, &[], &[a, b]);
        assert_eq!(w.on_top_of("red-01").as_deref(), Some("red-02"));
        assert_eq!(w.on_top_of("red-02"), None);
        assert!((w.top_of("red-02").unwrap() - 0.064).abs() < 1e-9);
    }
}
