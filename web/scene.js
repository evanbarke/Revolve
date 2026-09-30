// Revolve: a live 3D view of the simulated robot cell.
//
// Everything drawn comes from the simulator: place geometry, where each part
// rests, and the arm's commanded motion segments, each with a wall-clock start
// and duration. The view poses the arm with the same kinematics as the
// simulator (src/cell.rs) and interpolates the segments against the server
// clock, so it shows the motion as it happens. It never invents motion.
//
// World frame (simulator): x right, y forward, z up, metres. three.js: y up.

import * as THREE from "/vendor/three-r186/three.module.js";
import { OrbitControls } from "/vendor/three-r186/OrbitControls.js";

const V = (x, y, z) => new THREE.Vector3(x, z, -y);
const KIND_COLOR = { red_block: 0xd13b3b, blue_block: 0x2d63d6, green_block: 0x2e9a57 };
const BIN_TINT = { red_block: 0xd13b3b, blue_block: 0x2d63d6, green_block: 0x2e9a57, washer: 0x8a929c };
const dark = () => window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;

function partHeight(kind) {
  if (kind === "washer") return 0.008;
  if (kind in KIND_COLOR) return 0.032;
  return 0.03;
}

function ease(u) { return 0.5 - 0.5 * Math.cos(Math.PI * Math.min(1, Math.max(0, u))); }

function label(text, color) {
  const c = document.createElement("canvas");
  const ctx = c.getContext("2d");
  const px = 44;
  ctx.font = "600 " + px + "px system-ui, sans-serif";
  const w = Math.ceil(ctx.measureText(text).width) + 24;
  c.width = w; c.height = px + 20;
  ctx.font = "600 " + px + "px system-ui, sans-serif";
  ctx.fillStyle = dark() ? "rgba(20,24,30,0.72)" : "rgba(255,255,255,0.8)";
  ctx.beginPath(); ctx.roundRect(0, 0, w, px + 20, 14); ctx.fill();
  ctx.fillStyle = color || (dark() ? "#e6e9ee" : "#1b1f24");
  ctx.fillText(text, 12, px + 2);
  const tex = new THREE.CanvasTexture(c);
  tex.colorSpace = THREE.SRGBColorSpace;
  const sp = new THREE.Sprite(new THREE.SpriteMaterial({ map: tex, depthTest: false, transparent: true }));
  const h = 0.03;
  sp.scale.set(h * w / (px + 20), h, 1);
  sp.renderOrder = 10;
  return sp;
}

function placeName(p) {
  if (p.kind === "table") return "Pile";
  if (p.kind === "tray") return "Tray " + p.id.slice(5);
  const k = { "bin-red": "Red bin", "bin-blue": "Blue bin", "bin-green": "Green bin", "bin-washer": "Washer bin" };
  return k[p.id] || p.id;
}

class Arm {
  constructor(cell, ghost) {
    this.cell = cell;
    const mat = ghost
      ? new THREE.MeshStandardMaterial({ color: 0x38bdf8, transparent: true, opacity: 0.32, depthWrite: false })
      : new THREE.MeshStandardMaterial({ color: dark() ? 0xc9d1db : 0xe8ecf1, metalness: 0.35, roughness: 0.45 });
    this.mat = mat;
    const accent = ghost ? mat : new THREE.MeshStandardMaterial({ color: 0xf59e0b, metalness: 0.2, roughness: 0.5 });
    const joint = ghost ? mat : new THREE.MeshStandardMaterial({ color: 0x3b4552, metalness: 0.5, roughness: 0.4 });
    const cyl = (r) => new THREE.Mesh(new THREE.CylinderGeometry(r, r, 1, 20), mat);
    this.group = new THREE.Group();
    this.base = new THREE.Mesh(new THREE.CylinderGeometry(0.075, 0.085, 0.05, 28), joint);
    this.base.position.set(0, 0.025, 0);
    this.column = cyl(0.045);
    this.upper = cyl(0.03);
    this.fore = cyl(0.024);
    this.tool = new THREE.Mesh(new THREE.CylinderGeometry(0.012, 0.012, 1, 12), joint);
    this.shoulder = new THREE.Mesh(new THREE.SphereGeometry(0.045, 20, 14), accent);
    this.elbow = new THREE.Mesh(new THREE.SphereGeometry(0.036, 20, 14), accent);
    this.wrist = new THREE.Mesh(new THREE.SphereGeometry(0.026, 16, 12), joint);
    this.fingers = [0, 1].map(() => new THREE.Mesh(new THREE.BoxGeometry(0.008, 0.03, 0.022), joint));
    for (const m of [this.base, this.column, this.upper, this.fore, this.tool, this.shoulder, this.elbow, this.wrist, ...this.fingers]) {
      m.castShadow = !ghost;
      this.group.add(m);
    }
  }

  // Joint points for joint angles q (same kinematics as src/cell.rs).
  points(q) {
    const c = this.cell;
    const a1 = q[1], a2 = q[1] + q[2], a3 = a2 + q[3];
    const re = c.l1 * Math.cos(a1), ze = c.shoulder_h + c.l1 * Math.sin(a1);
    const rw = re + c.l2 * Math.cos(a2), zw = ze + c.l2 * Math.sin(a2);
    const rt = rw + c.l3 * Math.cos(a3), zt = zw + c.l3 * Math.sin(a3);
    const at = (r, z) => V(r * Math.cos(q[0]), r * Math.sin(q[0]), z);
    return { s: at(0, c.shoulder_h), e: at(re, ze), w: at(rw, zw), t: at(rt, zt), yaw: q[0] };
  }

  static link(mesh, a, b) {
    const d = new THREE.Vector3().subVectors(b, a);
    mesh.position.copy(a).addScaledVector(d, 0.5);
    mesh.scale.set(1, Math.max(d.length(), 1e-4), 1);
    mesh.quaternion.setFromUnitVectors(new THREE.Vector3(0, 1, 0), d.normalize());
  }

  pose(q, grip) {
    const p = this.points(q);
    Arm.link(this.column, V(0, 0, 0.05), p.s);
    Arm.link(this.upper, p.s, p.e);
    Arm.link(this.fore, p.e, p.w);
    Arm.link(this.tool, p.w, p.t);
    this.shoulder.position.copy(p.s);
    this.elbow.position.copy(p.e);
    this.wrist.position.copy(p.w);
    // Fingers open across the arm's plane.
    const side = V(-Math.sin(p.yaw), Math.cos(p.yaw), 0).normalize();
    const gap = 0.009 + 0.017 * grip;
    this.fingers.forEach((f, i) => {
      f.position.copy(p.t).addScaledVector(side, i ? gap : -gap);
      f.position.y -= 0.004;
      f.rotation.set(0, -p.yaw, 0);
    });
    return p.t;
  }
}

function partMesh(kind, observed, ghost) {
  const opts = ghost ? { transparent: true, opacity: 0.4, depthWrite: false } : {};
  let m;
  if (observed === "unknown") {
    m = new THREE.Mesh(new THREE.DodecahedronGeometry(0.016), new THREE.MeshStandardMaterial({ color: 0x6b7280, roughness: 0.9, ...opts }));
  } else if (kind === "washer") {
    const g = new THREE.TorusGeometry(0.0115, 0.0045, 12, 28);
    g.rotateX(Math.PI / 2);
    m = new THREE.Mesh(g, new THREE.MeshStandardMaterial({ color: 0xb8c0ca, metalness: 0.85, roughness: 0.28, ...opts }));
  } else {
    const g = new THREE.BoxGeometry(0.032, 0.032, 0.032);
    m = new THREE.Mesh(g, new THREE.MeshStandardMaterial({ color: KIND_COLOR[kind] || 0x888888, roughness: 0.55, ...opts }));
  }
  m.castShadow = !ghost;
  m.receiveShadow = !ghost;
  return m;
}

class Cell {
  constructor(el) {
    this.el = el;
    this.renderer = new THREE.WebGLRenderer({ antialias: true, alpha: false });
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
    this.renderer.shadowMap.enabled = true;
    this.renderer.shadowMap.type = THREE.PCFSoftShadowMap;
    el.append(this.renderer.domElement);
    this.scene = new THREE.Scene();
    this.scene.background = new THREE.Color(dark() ? 0x14181d : 0xeef1f5);
    this.camera = new THREE.PerspectiveCamera(38, 1, 0.01, 20);
    this.camera.position.copy(V(0.78, 1.12, 0.98));
    this.controls = new OrbitControls(this.camera, this.renderer.domElement);
    this.controls.target.copy(V(0.02, 0.22, 0.16));
    this.controls.enableDamping = true;
    this.controls.minDistance = 0.35;
    this.controls.maxDistance = 2.4;
    this.controls.maxPolarAngle = Math.PI * 0.47;
    this.controls.update();
    this.scene.add(new THREE.HemisphereLight(0xffffff, dark() ? 0x202428 : 0x9aa3ad, dark() ? 1.1 : 1.3));
    const sun = new THREE.DirectionalLight(0xffffff, 1.6);
    sun.position.copy(V(0.5, 0.2, 1.4));
    sun.castShadow = true;
    sun.shadow.mapSize.set(2048, 2048);
    Object.assign(sun.shadow.camera, { left: -0.8, right: 0.8, top: 0.8, bottom: -0.8, near: 0.1, far: 3 });
    this.scene.add(sun);
    const table = new THREE.Mesh(new THREE.BoxGeometry(1.5, 0.03, 1.0), new THREE.MeshStandardMaterial({ color: dark() ? 0x2b3139 : 0xd6dbe1, roughness: 0.9 }));
    table.position.copy(V(0, 0.28, -0.015));
    table.receiveShadow = true;
    this.scene.add(table);
    this.placesGroup = new THREE.Group();
    this.scene.add(this.placesGroup);
    this.partsGroup = new THREE.Group();
    this.scene.add(this.partsGroup);
    this.parts = new Map();
    this.placeMeshes = new Map();
    this.motions = [];
    this.offset = 0;
    this.layoutKey = "";
    this.arm = null;
    this.ghost = null;
    this.hud = null;
    const ro = new ResizeObserver(() => this.resize());
    ro.observe(el);
    this.resize();
    const loop = () => { this.frame(); requestAnimationFrame(loop); };
    requestAnimationFrame(loop);
  }

  resize() {
    const w = this.el.clientWidth || 600;
    const h = Math.max(260, Math.round(w * 0.62));
    this.renderer.setSize(w, h);
    this.camera.aspect = w / h;
    this.camera.updateProjectionMatrix();
  }

  buildPlaces(places) {
    const key = places.map((p) => p.id + p.available).join(",");
    if (key === this.layoutKey) return;
    this.layoutKey = key;
    this.placesGroup.clear();
    this.placeMeshes.clear();
    for (const p of places) {
      const g = new THREE.Group();
      g.position.copy(V(p.cx, p.cy, 0));
      g.rotation.y = p.yaw;
      const tint = p.kind === "bin" ? (BIN_TINT[p.accepts] || 0x777777) : p.kind === "tray" ? 0x5b6b7f : 0x9aa3ad;
      const mat = new THREE.MeshStandardMaterial({ color: tint, roughness: 0.7, transparent: p.kind === "bin", opacity: p.kind === "bin" ? 0.55 : 1 });
      const floorMat = new THREE.MeshStandardMaterial({ color: p.kind === "tray" ? (dark() ? 0x3a4656 : 0xc3cbd6) : (dark() ? 0x39404a : 0xc9ced6), roughness: 0.85 });
      if (p.kind === "table") {
        const disc = new THREE.Mesh(new THREE.CircleGeometry(p.w + 0.01, 40), new THREE.MeshStandardMaterial({ color: dark() ? 0x3a414b : 0xc4cad2, roughness: 0.95 }));
        disc.rotation.x = -Math.PI / 2;
        disc.position.y = 0.0006;
        disc.receiveShadow = true;
        g.add(disc);
      } else {
        const floor = new THREE.Mesh(new THREE.BoxGeometry(p.w, 0.004, p.d), floorMat);
        floor.position.y = 0.002;
        floor.receiveShadow = true;
        g.add(floor);
        const t = 0.004;
        for (const [sx, sz, lx, lz] of [[p.w, t, 0, p.d / 2], [p.w, t, 0, -p.d / 2], [t, p.d, p.w / 2, 0], [t, p.d, -p.w / 2, 0]]) {
          const wall = new THREE.Mesh(new THREE.BoxGeometry(sx, p.wall, sz), mat);
          wall.position.set(lx, p.wall / 2, lz);
          wall.castShadow = true;
          g.add(wall);
        }
      }
      const tag = label(placeName(p) + (p.available ? "" : " (not available)"), p.available ? null : "#ef4444");
      tag.position.set(0, (p.wall || 0.01) + 0.035, p.kind === "table" ? 0 : p.d / 2 + 0.01);
      g.add(tag);
      this.placesGroup.add(g);
      this.placeMeshes.set(p.id, { group: g, place: p, mat });
    }
  }

  // Merge a scene response from the server.
  update(sc) {
    this.cell = sc.cell;
    this.offset = sc.now_ms - Date.now();
    this.buildPlaces(sc.places || []);
    if (!this.arm) {
      this.arm = new Arm(this.cell, false);
      this.scene.add(this.arm.group);
    }
    this.restArm = sc.arm;
    for (const m of sc.motions || []) {
      if (!this.motions.length || m.seq > this.motions[this.motions.length - 1].seq) this.motions.push(m);
    }
    if (this.motions.length > 400) this.motions.splice(0, this.motions.length - 400);
    this.lastSeq = sc.last_seq;
    const seen = new Set();
    for (const o of sc.objects || []) {
      seen.add(o.id);
      let e = this.parts.get(o.id);
      if (!e) {
        e = { mesh: partMesh(o.kind, o.observed, false), kind: o.kind };
        this.parts.set(o.id, e);
        this.partsGroup.add(e.mesh);
      }
      e.at = o.at;
      e.pos = o.x == null ? null : V(o.x, o.y, o.z);
      e.mesh.visible = o.at !== "outside";
    }
    for (const [id, e] of this.parts) if (!seen.has(id)) { this.partsGroup.remove(e.mesh); this.parts.delete(id); }
  }

  // The arm at time t (server clock) from the logged segments.
  armAt(t, motions, rest) {
    let seg = null;
    for (let i = motions.length - 1; i >= 0; i--) {
      if (motions[i].at_ms <= t) { seg = motions[i]; break; }
    }
    if (!seg) return { q: rest.q, grip: rest.grip, seg: null };
    const u = seg.play_ms > 0 ? ease((t - seg.at_ms) / seg.play_ms) : 1;
    const q = seg.from.q.map((a, i) => a + (seg.to.q[i] - a) * u);
    const grip = seg.from.grip + (seg.to.grip - seg.from.grip) * u;
    return { q, grip, seg: u < 1 ? seg : seg };
  }

  frame() {
    this.controls.update();
    if (this.arm && this.restArm) {
      const t = Date.now() + this.offset;
      const a = this.armAt(t, this.motions, this.restArm);
      const tip = this.arm.pose(a.q, a.grip);
      const moving = a.seg && t < a.seg.at_ms + a.seg.play_ms;
      const carrying = a.seg ? a.seg.carrying : null;
      for (const [id, e] of this.parts) {
        const inHand = carrying === id || (e.at === "gripper" && (!carrying || carrying === id));
        if (inHand) {
          e.mesh.position.set(tip.x, tip.y - partHeight(e.kind) / 2, tip.z);
          e.mesh.visible = true;
        } else if (e.pos) {
          e.mesh.position.copy(e.pos);
        }
      }
      // Highlight where the arm is heading.
      const target = moving ? this.nearestPlace(a.seg.to.q) : null;
      for (const [id, pm] of this.placeMeshes) pm.mat.emissive && pm.mat.emissive.setHex(id === target ? 0x996600 : 0x000000);
      if (this.hud) this.hud(moving, a.seg);
    }
    if (this.ghost) this.ghostFrame();
    this.renderer.render(this.scene, this.camera);
  }

  nearestPlace(q) {
    if (!this.arm) return null;
    const tip = this.arm.points(q).t;
    let best = null, bd = 0.12;
    for (const [id, pm] of this.placeMeshes) {
      const p = pm.place;
      const d = Math.hypot(tip.x - p.cx, -tip.z - p.cy);
      if (d < bd) { bd = d; best = id; }
    }
    return best;
  }

  // Replay a rehearsal: a see-through arm runs the logged segments at `rate`x.
  preview(motions, rate) {
    this.stopPreview();
    if (!motions || !motions.length || !this.cell) return;
    const ghost = new Arm(this.cell, true);
    this.scene.add(ghost.group);
    const t0 = Date.now();
    let at = 0;
    const segs = motions.map((m) => { const s = { ...m, at_ms: at, play_ms: m.nominal_ms / rate }; at += s.play_ms; return s; });
    const ghostParts = new Map();
    const group = new THREE.Group();
    this.scene.add(group);
    this.ghost = { arm: ghost, segs, t0, total: at, parts: ghostParts, group };
  }

  stopPreview() {
    if (!this.ghost) return;
    this.scene.remove(this.ghost.arm.group);
    this.scene.remove(this.ghost.group);
    this.ghost = null;
  }

  ghostFrame() {
    const g = this.ghost;
    const t = Math.min(Date.now() - g.t0, g.total);
    const a = this.armAt(t, g.segs, g.segs[0].from);
    const tip = g.arm.pose(a.q, a.grip);
    const id = a.seg && t < a.seg.at_ms + a.seg.play_ms ? a.seg.carrying : null;
    if (id) {
      let gp = g.parts.get(id);
      if (!gp) {
        const real = this.parts.get(id);
        gp = partMesh(real ? real.kind : "unknown", "", true);
        g.parts.set(id, gp);
        g.group.add(gp);
      }
      gp.position.set(tip.x, tip.y - partHeight((this.parts.get(id) || {}).kind) / 2, tip.z);
      gp.userData.id = id;
    }
    // When the replay ends, the see-through arm fades away; the parts it moved
    // stay where the rehearsal left them (the predicted result).
    if (t >= g.total) {
      if (!g.doneAt) g.doneAt = Date.now();
      const f = Math.min(1, (Date.now() - g.doneAt) / 1200);
      g.arm.mat.opacity = 0.32 * (1 - f);
      if (f >= 1 && g.arm.group.parent) this.scene.remove(g.arm.group);
    }
  }
}

let cell = null;
window.RevolveCell = {
  mount(el) {
    try { cell = new Cell(el); return true; } catch (e) { console.warn("3D view unavailable:", e); return false; }
  },
  update(sc) { if (cell) cell.update(sc); },
  reset() { if (cell) { cell.motions = []; cell.parts.forEach((e) => cell.partsGroup.remove(e.mesh)); cell.parts.clear(); cell.layoutKey = ""; cell.stopPreview(); } },
  preview(motions, rate) { if (cell) cell.preview(motions, rate || 4); },
  stopPreview() { if (cell) cell.stopPreview(); },
  onHud(fn) { if (cell) cell.hud = fn; },
  lastSeq() { return cell ? cell.lastSeq || 0 : 0; },
  ready: true,
};
window.dispatchEvent(new Event("revolve-cell-ready"));
