//! DOOM-like raycaster in the terminal.
//! Rendering uses truecolor half-blocks (▀) so each terminal cell holds two square pixels.

use crossterm::{
    cursor,
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::{Duration, Instant};

const TEX: usize = 64;
const PLANE: f64 = 0.75; // tan(FOV/2) ≈ 74° horizontal FOV
const HUD_ROWS: usize = 3;

// '1'..'4' walls, P player, E imp, B demon, H medikit, A ammo
const MAP: [&str; 24] = [
    "11111111111111111111111111111111",
    "1P.....1.........3............31",
    "1......1....E....3....E.......31",
    "1..H...1.........333333..33333.1",
    "1......1.....................A.1",
    "1111.111111.11111.....B........1",
    "1..............1..........H....1",
    "1..E...........1...4444..4444..1",
    "1..............1...4........4..1",
    "1....2222......1...4..B.....4..1",
    "1....2..2..A...1...4........4..1",
    "1....2..2......1...4444..4444..1",
    "1....22.2..........E...........1",
    "1..............1...............1",
    "111111.11111111111111.1111111111",
    "1...........3..........1.......1",
    "1..E........3.....E....1...B...1",
    "1...........3..........1.......1",
    "1.....A.....3..H.......1...A...1",
    "1..........................E...1",
    "1...B.......3..........1.......1",
    "1...........3.....E....1..H....1",
    "1...........3..........1.......1",
    "11111111111111111111111111111111",
];

// ---------------------------------------------------------------- colors / noise

fn rgb(r: f64, g: f64, b: f64) -> u32 {
    let c = |x: f64| x.clamp(0.0, 255.0) as u32;
    (c(r) << 16) | (c(g) << 8) | c(b)
}
fn hex(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}
fn split(c: u32) -> (f64, f64, f64) {
    (((c >> 16) & 255) as f64, ((c >> 8) & 255) as f64, (c & 255) as f64)
}
fn shade(c: u32, f: f64) -> u32 {
    let (r, g, b) = split(c);
    rgb(r * f, g * f, b * f)
}
fn mix(a: u32, b: u32, t: f64) -> u32 {
    let (r1, g1, b1) = split(a);
    let (r2, g2, b2) = split(b);
    rgb(r1 + (r2 - r1) * t, g1 + (g2 - g1) * t, b1 + (b2 - b1) * t)
}

fn hash(x: i32, y: i32, s: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x27d4_eb2d)
        ^ (y as u32).wrapping_mul(0x1656_67b1)
        ^ s.wrapping_mul(0x9e37_79b9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^ (h >> 16)
}
fn noise(x: i32, y: i32, s: u32) -> f64 {
    (hash(x, y, s) & 0xffff) as f64 / 65535.0
}
fn smooth_noise(x: f64, y: f64, s: u32) -> f64 {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let a = noise(xi, yi, s) + (noise(xi + 1, yi, s) - noise(xi, yi, s)) * sx;
    let b = noise(xi, yi + 1, s) + (noise(xi + 1, yi + 1, s) - noise(xi, yi + 1, s)) * sx;
    a + (b - a) * sy
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.f()
    }
    fn int(&mut self, a: i32, b: i32) -> i32 {
        a + (self.next() % (b - a + 1) as u64) as i32
    }
}

// ---------------------------------------------------------------- textures

/// 0 brick, 1 stone, 2 tech, 3 marble (walls '1'..'4'), 4 floor, 5 ceiling
fn make_textures() -> Vec<Vec<u32>> {
    let mut t = vec![vec![0u32; TEX * TEX]; 6];
    for y in 0..TEX {
        for x in 0..TEX {
            let (xi, yi) = (x as i32, y as i32);
            let n = noise(xi, yi, 1);
            let i = y * TEX + x;

            // brick
            let row = y / 16;
            let off = if row % 2 == 0 { 0 } else { 16 };
            let bx = (x + off) % 32;
            let id = ((x + off) / 32) as i32 + row as i32 * 7;
            t[0][i] = if y % 16 < 2 || bx < 2 {
                rgb(70.0 + n * 25.0, 65.0 + n * 25.0, 60.0 + n * 25.0)
            } else {
                let v = 0.7 + noise(id, 0, 9) * 0.3 + n * 0.15 + if y % 16 == 2 { 0.15 } else { 0.0 };
                rgb(150.0 * v, 58.0 * v, 38.0 * v)
            };

            // stone blocks
            let soff = if (y / 32) % 2 == 0 { 0 } else { 16 };
            let sx = (x + soff) % 32;
            let sid = ((x + soff) / 32 + (y / 32) * 5) as i32;
            t[1][i] = if y % 32 < 2 || sx < 2 {
                hex(38, 38, 40)
            } else {
                let mut v = 0.6 + noise(sid, 3, 4) * 0.2 + smooth_noise(x as f64 / 4.0, y as f64 / 4.0, 7) * 0.3 + n * 0.08;
                if y % 32 == 2 || sx == 2 {
                    v += 0.25;
                }
                rgb(120.0 * v, 118.0 * v, 110.0 * v)
            };

            // tech panels
            let (px, py) = (x % 32, y % 32);
            t[2][i] = if (44..48).contains(&y) {
                rgb(40.0, 200.0 * (0.85 + 0.15 * n), 230.0)
            } else if px < 1 || py < 1 {
                hex(30, 34, 44)
            } else if px == 1 || py == 1 {
                hex(125, 135, 155)
            } else if (px == 4 || px == 27) && (py == 4 || py == 27) {
                hex(200, 200, 210)
            } else {
                let v = 0.8 + n * 0.15;
                rgb(75.0 * v, 85.0 * v, 105.0 * v)
            };

            // green marble with gold frame
            let m = smooth_noise(x as f64 / 8.0, y as f64 / 8.0, 5);
            let vein = (x as f64 * 0.12 + y as f64 * 0.05 + m * 7.0).sin().abs();
            t[3][i] = if x < 3 || x > 60 || y < 3 || y > 60 {
                let v = 0.8 + n * 0.3;
                rgb(160.0 * v, 125.0 * v, 45.0 * v)
            } else if vein < 0.1 {
                hex(150, 200, 150)
            } else {
                let v = 0.6 + 0.4 * m + n * 0.1;
                rgb(40.0 * v, 105.0 * v, 55.0 * v)
            };

            // floor tiles
            let (fx, fy) = (x % 32, y % 32);
            let tid = (x / 32 + (y / 32) * 2) as i32;
            t[4][i] = if fx < 1 || fy < 1 {
                hex(35, 30, 25)
            } else {
                let v = 0.75 + 0.15 * n + 0.15 * noise(tid, 1, 3);
                rgb(95.0 * v, 82.0 * v, 62.0 * v)
            };

            // ceiling with lights
            t[5][i] = if (24..40).contains(&x) && (24..40).contains(&y) {
                hex(240, 235, 200)
            } else if x % 16 == 0 || y % 16 == 0 {
                hex(28, 28, 30)
            } else {
                let v = 0.85 + n * 0.1;
                rgb(55.0 * v, 55.0 * v, 60.0 * v)
            };
        }
    }
    t
}

// ---------------------------------------------------------------- world

struct World {
    w: i32,
    h: i32,
    g: Vec<u8>,
}

impl World {
    fn cell(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            1
        } else {
            self.g[(y * self.w + x) as usize]
        }
    }
    fn solid(&self, x: f64, y: f64) -> bool {
        self.cell(x.floor() as i32, y.floor() as i32) != 0
    }
    fn free(&self, x: f64, y: f64, r: f64) -> bool {
        !self.solid(x - r, y - r) && !self.solid(x + r, y - r) && !self.solid(x - r, y + r) && !self.solid(x + r, y + r)
    }
    /// DDA. Returns (t, cell, hit_x_side) where hit = origin + t * dir.
    fn cast(&self, ox: f64, oy: f64, dx: f64, dy: f64) -> (f64, u8, bool) {
        let (mut mx, mut my) = (ox.floor() as i32, oy.floor() as i32);
        let ddx = if dx == 0.0 { 1e30 } else { (1.0 / dx).abs() };
        let ddy = if dy == 0.0 { 1e30 } else { (1.0 / dy).abs() };
        let (sx, mut sdx) = if dx < 0.0 { (-1, (ox - mx as f64) * ddx) } else { (1, (mx as f64 + 1.0 - ox) * ddx) };
        let (sy, mut sdy) = if dy < 0.0 { (-1, (oy - my as f64) * ddy) } else { (1, (my as f64 + 1.0 - oy) * ddy) };
        for _ in 0..256 {
            let (t, side_x);
            if sdx < sdy {
                mx += sx;
                t = sdx;
                sdx += ddx;
                side_x = true;
            } else {
                my += sy;
                t = sdy;
                sdy += ddy;
                side_x = false;
            }
            let c = self.cell(mx, my);
            if c != 0 {
                return (t, c, side_x);
            }
        }
        (1e9, 1, true)
    }
    fn los(&self, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        let (dx, dy) = (bx - ax, by - ay);
        let d = (dx * dx + dy * dy).sqrt();
        if d < 1e-6 {
            return true;
        }
        self.cast(ax, ay, dx / d, dy / d).0 >= d
    }
}

// ---------------------------------------------------------------- entities

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Imp,
    Demon,
    Fireball,
    Health,
    Ammo,
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    Idle,
    Chase,
    Attack,
    Pain,
    Dying,
    Dead,
    Fly,
    Explode,
    Item,
}

#[derive(Clone)]
struct Ent {
    kind: Kind,
    st: St,
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    hp: i32,
    timer: f64,
    cool: f64,
    anim: f64,
    fired: bool,
    remove: bool,
}

impl Ent {
    fn new(kind: Kind, x: f64, y: f64) -> Self {
        let (st, hp) = match kind {
            Kind::Imp => (St::Idle, 60),
            Kind::Demon => (St::Idle, 150),
            Kind::Fireball => (St::Fly, 0),
            _ => (St::Item, 0),
        };
        Ent { kind, st, x, y, vx: 0.0, vy: 0.0, hp, timer: 0.0, cool: 0.0, anim: 0.0, fired: false, remove: false }
    }
    fn is_monster(&self) -> bool {
        matches!(self.kind, Kind::Imp | Kind::Demon)
    }
    fn alive_monster(&self) -> bool {
        self.is_monster() && !matches!(self.st, St::Dying | St::Dead)
    }
}

// ---------------------------------------------------------------- input

struct Input {
    held: HashMap<KeyCode, Instant>,
    enhanced: bool,
    pressed: Vec<KeyCode>,
}

fn norm(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        c => c,
    }
}

impl Input {
    fn on_key(&mut self, k: KeyEvent) {
        let code = norm(k.code);
        let now = Instant::now();
        match k.kind {
            KeyEventKind::Press | KeyEventKind::Repeat => {
                // Without key-release events (legacy terminals) we fake "held" with a timeout:
                // long enough on first press to bridge the OS key-repeat delay.
                let ext = if self.enhanced {
                    1000
                } else if self.down(code) {
                    130
                } else {
                    450
                };
                self.held.insert(code, now + Duration::from_millis(ext));
                if k.kind == KeyEventKind::Press {
                    self.pressed.push(code);
                }
            }
            KeyEventKind::Release => {
                self.held.remove(&code);
            }
        }
    }
    fn down(&self, code: KeyCode) -> bool {
        self.held.get(&code).is_some_and(|&t| Instant::now() < t)
    }
}

// ---------------------------------------------------------------- game

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Title,
    Playing,
    Dead,
    Won,
}

struct Player {
    x: f64,
    y: f64,
    a: f64,
    health: i32,
    bullets: i32,
    shells: i32,
    weapon: usize,
    cool: f64,
    trigger: f64,
    flash: f64,
    hurt: f64,
    bonus: f64,
    bob: f64,
    moving: f64,
}

struct Game {
    world: World,
    ents: Vec<Ent>,
    p: Player,
    mode: Mode,
    rng: Rng,
    kills: u32,
    total: u32,
    msg: String,
    msg_t: f64,
    minimap: bool,
    time: f64,
}

impl Game {
    fn new(seed: u64) -> Self {
        let h = MAP.len() as i32;
        let w = MAP.iter().map(|r| r.len()).max().unwrap() as i32;
        let mut g = vec![0u8; (w * h) as usize];
        let mut ents = Vec::new();
        let (mut px, mut py) = (1.5, 1.5);
        for (y, row) in MAP.iter().enumerate() {
            for x in 0..w as usize {
                let c = row.as_bytes().get(x).copied().unwrap_or(b'1');
                let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
                match c {
                    b'1'..=b'4' => g[y * w as usize + x] = c - b'0',
                    b'P' => (px, py) = (fx, fy),
                    b'E' => ents.push(Ent::new(Kind::Imp, fx, fy)),
                    b'B' => ents.push(Ent::new(Kind::Demon, fx, fy)),
                    b'H' => ents.push(Ent::new(Kind::Health, fx, fy)),
                    b'A' => ents.push(Ent::new(Kind::Ammo, fx, fy)),
                    _ => {}
                }
            }
        }
        let total = ents.iter().filter(|e| e.is_monster()).count() as u32;
        Game {
            world: World { w, h, g },
            ents,
            p: Player {
                x: px,
                y: py,
                a: 0.0,
                health: 100,
                bullets: 50,
                shells: 10,
                weapon: 0,
                cool: 0.0,
                trigger: 0.0,
                flash: 0.0,
                hurt: 0.0,
                bonus: 0.0,
                bob: 0.0,
                moving: 0.0,
            },
            mode: Mode::Title,
            rng: Rng(seed | 1),
            kills: 0,
            total,
            msg: String::new(),
            msg_t: 0.0,
            minimap: true,
            time: 0.0,
        }
    }

    fn say(&mut self, s: &str) {
        self.msg = s.to_string();
        self.msg_t = 2.5;
    }

    fn update(&mut self, dt: f64, inp: &mut Input) {
        self.time += dt;
        self.msg_t -= dt;
        let pressed: Vec<KeyCode> = inp.pressed.drain(..).collect();
        let pr = |k: KeyCode| pressed.contains(&k);
        {
            let p = &mut self.p;
            p.hurt = (p.hurt - dt * 1.5).max(0.0);
            p.bonus = (p.bonus - dt * 1.5).max(0.0);
            p.flash = (p.flash - dt).max(0.0);
            p.cool -= dt;
            p.trigger -= dt;
        }

        match self.mode {
            Mode::Title => {
                self.p.a += dt * 0.25;
                if pr(KeyCode::Enter) || pr(KeyCode::Char(' ')) {
                    self.p.a = 0.0;
                    self.mode = Mode::Playing;
                    self.say("Extermine tous les démons !");
                }
                return;
            }
            Mode::Dead | Mode::Won => {
                if pr(KeyCode::Char('r')) {
                    *self = Game::new(self.rng.next());
                    self.mode = Mode::Playing;
                    return;
                }
                self.update_ents(dt);
                return;
            }
            Mode::Playing => {}
        }

        if pr(KeyCode::Char('1')) {
            self.p.weapon = 0;
        }
        if pr(KeyCode::Char('2')) {
            self.p.weapon = 1;
        }
        if pr(KeyCode::Char('m')) || pr(KeyCode::Tab) {
            self.minimap = !self.minimap;
        }

        // movement
        let k = |c: char| inp.down(KeyCode::Char(c));
        let axis = |pos: bool, neg: bool| pos as i32 as f64 - neg as i32 as f64;
        // both QWERTY (WASD) and AZERTY (ZQSD) layouts
        let fwd = axis(k('w') || k('z') || inp.down(KeyCode::Up), k('s') || inp.down(KeyCode::Down));
        let strafe = axis(k('d'), k('a') || k('q'));
        let turn = axis(inp.down(KeyCode::Right), inp.down(KeyCode::Left));

        let p = &mut self.p;
        p.a += turn * 2.6 * dt;
        let (dx, dy) = (p.a.cos(), p.a.sin());
        let (mut mx, mut my) = (dx * fwd - dy * strafe, dy * fwd + dx * strafe);
        let len = (mx * mx + my * my).sqrt();
        if len > 0.0 {
            let s = 3.8 * dt / len;
            mx *= s;
            my *= s;
            if self.world.free(p.x + mx, p.y, 0.25) {
                p.x += mx;
            }
            if self.world.free(p.x, p.y + my, 0.25) {
                p.y += my;
            }
            p.bob += dt * 9.0;
        }
        let target = if len > 0.0 { 1.0 } else { 0.0 };
        p.moving += (target - p.moving) * (dt * 8.0).min(1.0);

        // shooting
        let fire_keys = [KeyCode::Char(' '), KeyCode::Char('f')];
        if fire_keys.iter().any(|&c| pr(c)) {
            p.trigger = 0.25;
        }
        let holding = inp.enhanced && fire_keys.iter().any(|&c| inp.down(c));
        if (p.trigger > 0.0 || holding) && p.cool <= 0.0 {
            p.trigger = 0.0;
            self.shoot();
        }

        // pickups
        let (px, py) = (self.p.x, self.p.y);
        for i in 0..self.ents.len() {
            let e = &self.ents[i];
            if e.st != St::Item || (e.x - px).hypot(e.y - py) > 0.55 {
                continue;
            }
            match e.kind {
                Kind::Health if self.p.health < 100 => {
                    self.p.health = (self.p.health + 25).min(100);
                    self.ents[i].remove = true;
                    self.p.bonus = 0.6;
                    self.say("Medikit ramassé (+25)");
                }
                Kind::Ammo => {
                    self.p.bullets += 20;
                    self.p.shells += 4;
                    self.ents[i].remove = true;
                    self.p.bonus = 0.6;
                    self.say("Munitions ramassées");
                }
                _ => {}
            }
        }

        self.update_ents(dt);

        if self.mode == Mode::Playing && self.kills == self.total {
            self.mode = Mode::Won;
            self.say("Niveau nettoyé. Respect.");
        }
    }

    fn shoot(&mut self) {
        let (pellets, spread, dmin, dmax);
        if self.p.weapon == 0 {
            if self.p.bullets <= 0 {
                self.say("Plus de balles !");
                if self.p.shells > 0 {
                    self.p.weapon = 1;
                }
                return;
            }
            self.p.bullets -= 1;
            self.p.cool = 0.32;
            (pellets, spread, dmin, dmax) = (1, 0.012, 12, 22);
        } else {
            if self.p.shells <= 0 {
                self.say("Plus de cartouches !");
                if self.p.bullets > 0 {
                    self.p.weapon = 0;
                }
                return;
            }
            self.p.shells -= 1;
            self.p.cool = 0.85;
            (pellets, spread, dmin, dmax) = (7, 0.09, 7, 14);
        }
        self.p.flash = 0.12;
        let (px, py, a) = (self.p.x, self.p.y, self.p.a);

        for _ in 0..pellets {
            let ang = a + self.rng.range(-spread, spread);
            let (rx, ry) = (ang.cos(), ang.sin());
            let wall = self.world.cast(px, py, rx, ry).0;
            let mut best: Option<(f64, usize)> = None;
            for (i, e) in self.ents.iter().enumerate() {
                if !e.alive_monster() {
                    continue;
                }
                let (ex, ey) = (e.x - px, e.y - py);
                let t = ex * rx + ey * ry;
                if t <= 0.0 || t > wall {
                    continue;
                }
                let perp = (ex * ry - ey * rx).abs();
                let rad = if e.kind == Kind::Imp { 0.3 } else { 0.4 };
                if perp < rad && best.is_none_or(|(bt, _)| t < bt) {
                    best = Some((t, i));
                }
            }
            if let Some((_, i)) = best {
                let d = self.rng.int(dmin, dmax);
                self.damage(i, d);
            }
        }

        // gunfire wakes up everything nearby that can see you
        for e in self.ents.iter_mut() {
            if e.alive_monster()
                && e.st == St::Idle
                && (e.x - px).hypot(e.y - py) < 15.0
                && self.world.los(e.x, e.y, px, py)
            {
                e.st = St::Chase;
            }
        }
    }

    fn damage(&mut self, i: usize, d: i32) {
        let pain = if self.ents[i].kind == Kind::Imp { 0.7 } else { 0.45 };
        let roll = self.rng.f();
        let e = &mut self.ents[i];
        e.hp -= d;
        if e.hp <= 0 {
            e.st = St::Dying;
            e.timer = 0.5;
            self.kills += 1;
        } else if roll < pain {
            e.st = St::Pain;
            e.timer = 0.22;
        } else if e.st == St::Idle {
            e.st = St::Chase;
        }
    }

    fn update_ents(&mut self, dt: f64) {
        let mut spawned = Vec::new();
        let mut dmg = 0;
        let (px, py) = (self.p.x, self.p.y);
        let active = self.mode == Mode::Playing;
        for e in self.ents.iter_mut() {
            match e.kind {
                Kind::Imp | Kind::Demon => {
                    update_monster(e, &self.world, px, py, active, &mut self.rng, dt, &mut spawned, &mut dmg)
                }
                Kind::Fireball => {
                    if e.st == St::Fly {
                        let (nx, ny) = (e.x + e.vx * dt, e.y + e.vy * dt);
                        if self.world.solid(nx, ny) {
                            e.st = St::Explode;
                            e.timer = 0.35;
                        } else {
                            e.x = nx;
                            e.y = ny;
                            if active && (nx - px).hypot(ny - py) < 0.4 {
                                dmg += self.rng.int(8, 18);
                                e.st = St::Explode;
                                e.timer = 0.35;
                            }
                        }
                    } else {
                        e.timer -= dt;
                        if e.timer <= 0.0 {
                            e.remove = true;
                        }
                    }
                }
                _ => {}
            }
        }
        self.ents.retain(|e| !e.remove);
        self.ents.extend(spawned);
        if dmg > 0 && active {
            self.p.health -= dmg;
            self.p.hurt = (self.p.hurt + 0.5).min(1.0);
            if self.p.health <= 0 {
                self.p.health = 0;
                self.mode = Mode::Dead;
                self.say("Tu es mort. Appuie sur R.");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_monster(
    e: &mut Ent,
    w: &World,
    px: f64,
    py: f64,
    active: bool,
    rng: &mut Rng,
    dt: f64,
    spawned: &mut Vec<Ent>,
    dmg: &mut i32,
) {
    let (dx, dy) = (px - e.x, py - e.y);
    let dist = dx.hypot(dy).max(1e-6);
    e.cool -= dt;
    match e.st {
        St::Dying => {
            e.timer -= dt;
            if e.timer <= 0.0 {
                e.st = St::Dead;
            }
        }
        St::Pain => {
            e.timer -= dt;
            if e.timer <= 0.0 {
                e.st = St::Chase;
            }
        }
        St::Idle => {
            if active && dist < 16.0 && w.los(e.x, e.y, px, py) {
                e.st = St::Chase;
                e.cool = rng.range(0.5, 1.5);
            }
        }
        St::Attack => {
            e.timer -= dt;
            if e.kind == Kind::Imp && !e.fired && e.timer < 0.25 {
                e.fired = true;
                if active {
                    let ang = dy.atan2(dx) + rng.range(-0.05, 0.05);
                    let (ux, uy) = (ang.cos(), ang.sin());
                    let mut f = Ent::new(Kind::Fireball, e.x + ux * 0.4, e.y + uy * 0.4);
                    f.vx = ux * 6.0;
                    f.vy = uy * 6.0;
                    spawned.push(f);
                }
            }
            if e.kind == Kind::Demon && !e.fired && e.timer < 0.2 {
                e.fired = true;
                if active && dist < 1.3 {
                    *dmg += rng.int(6, 14);
                }
            }
            if e.timer <= 0.0 {
                e.st = St::Chase;
            }
        }
        St::Chase => {
            e.anim += dt;
            if !active {
                return;
            }
            if e.kind == Kind::Imp && e.cool <= 0.0 && dist < 14.0 && w.los(e.x, e.y, px, py) {
                e.st = St::Attack;
                e.timer = 0.6;
                e.fired = false;
                e.cool = rng.range(1.5, 3.2);
                return;
            }
            if e.kind == Kind::Demon && e.cool <= 0.0 && dist < 1.1 {
                e.st = St::Attack;
                e.timer = 0.45;
                e.fired = false;
                e.cool = 0.9;
                return;
            }
            if dist > 0.8 {
                let speed = if e.kind == Kind::Imp { 1.3 } else { 2.4 };
                let (mx, my) = (dx / dist * speed * dt, dy / dist * speed * dt);
                if w.free(e.x + mx, e.y, 0.3) {
                    e.x += mx;
                }
                if w.free(e.x, e.y + my, 0.3) {
                    e.y += my;
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------- sprites (procedural)

/// < 1.0 means inside the ellipse
fn ell(u: f64, v: f64, cx: f64, cy: f64, rx: f64, ry: f64) -> f64 {
    let (a, b) = ((u - cx) / rx, (v - cy) / ry);
    a * a + b * b
}

fn imp(u: f64, v: f64, attack: bool, step: bool) -> Option<u32> {
    let skin = |k: f64| rgb(140.0 * k, 88.0 * k, 50.0 * k);
    for ex in [0.455, 0.545] {
        if ell(u, v, ex, 0.19, 0.025, 0.018) < 1.0 {
            return Some(hex(255, 70, 20));
        }
    }
    if v > 0.235 && v < 0.255 && (u - 0.5).abs() < 0.04 {
        return Some(hex(50, 10, 5));
    }
    let hd = ell(u, v, 0.5, 0.2, 0.095, 0.1);
    if hd < 1.0 {
        return Some(skin(1.0 - 0.35 * hd));
    }
    for (hx, s) in [(0.43, -1.0), (0.57, 1.0)] {
        let t = (0.13 - v) / 0.09;
        if t > 0.0 && t < 1.0 && (u - (hx + s * t * 0.04)).abs() < 0.018 * (1.0 - t) + 0.004 {
            return Some(hex(220, 210, 180));
        }
    }
    if attack && ell(u, v, 0.5, 0.08, 0.08, 0.06) < 1.0 {
        return Some(hex(255, 190, 60));
    }
    let arms = if attack { [(0.3, 0.22), (0.7, 0.22)] } else { [(0.31, 0.46), (0.69, 0.46)] };
    for (ax, ay) in arms {
        let d = ell(u, v, ax, ay, 0.055, 0.17);
        if d < 1.0 {
            return Some(skin(0.85 - 0.3 * d));
        }
    }
    let b = ell(u, v, 0.5, 0.47, 0.17, 0.2);
    if b < 1.0 {
        let rib = if (u - 0.5).abs() < 0.09 && (v * 55.0).sin() > 0.6 { 0.75 } else { 1.0 };
        return Some(skin((1.0 - 0.35 * b) * rib));
    }
    let (l_end, r_end) = if step { (0.96, 1.0) } else { (1.0, 0.96) };
    if v > 0.62 && (((u - 0.42).abs() < 0.05 && v < l_end) || ((u - 0.58).abs() < 0.05 && v < r_end)) {
        return Some(skin(0.7));
    }
    None
}

fn demon(u: f64, v: f64, attack: bool, step: bool) -> Option<u32> {
    let skin = |k: f64| rgb(225.0 * k, 120.0 * k, 135.0 * k);
    for ex in [0.4, 0.6] {
        if ell(u, v, ex, 0.3, 0.03, 0.025) < 1.0 {
            return Some(hex(255, 230, 60));
        }
    }
    let m = ell(u, v, 0.5, 0.45, 0.15, if attack { 0.1 } else { 0.055 });
    if m < 1.0 {
        let tooth = m > 0.45 && (u * 60.0).sin() > 0.0;
        return Some(if tooth { hex(240, 235, 210) } else { hex(100, 0, 0) });
    }
    let hd = ell(u, v, 0.5, 0.38, 0.22, 0.17);
    if hd < 1.0 {
        return Some(skin(1.0 - 0.35 * hd));
    }
    let arm_y = if attack { 0.48 } else { 0.6 };
    for ax in [0.17, 0.83] {
        let d = ell(u, v, ax, arm_y, 0.07, 0.14);
        if d < 1.0 {
            return Some(skin(0.8 - 0.3 * d));
        }
    }
    let b = ell(u, v, 0.5, 0.62, 0.34, 0.24);
    if b < 1.0 {
        return Some(skin(0.95 - 0.4 * b));
    }
    let (l_end, r_end) = if step { (0.96, 1.0) } else { (1.0, 0.96) };
    if v > 0.78 && (((u - 0.35).abs() < 0.075 && v < l_end) || ((u - 0.65).abs() < 0.075 && v < r_end)) {
        return Some(skin(0.6));
    }
    None
}

fn corpse(u: f64, v: f64, is_demon: bool) -> Option<u32> {
    let e = ell(u, v, 0.5, 0.94, 0.4, 0.06);
    if e < 1.0 {
        return Some(rgb(130.0 * (1.0 - 0.4 * e), 15.0, 10.0));
    }
    if ell(u, v, 0.47, 0.88, 0.17, 0.07) < 1.0 {
        return Some(if is_demon { hex(170, 90, 100) } else { hex(110, 70, 40) });
    }
    None
}

fn fireball(u: f64, v: f64, t: f64, explode: bool, timer: f64) -> Option<u32> {
    let d = ell(u, v, 0.5, 0.5, 0.5, 0.5);
    if d >= 1.0 {
        return None;
    }
    let n = noise((u * 12.0) as i32, (v * 12.0) as i32, (t * 25.0) as u32);
    if explode {
        let p = 1.0 - timer / 0.35;
        if n < p * 0.9 {
            return None;
        }
        let k = 1.0 - d;
        return Some(rgb(255.0, 90.0 + 160.0 * k * (1.0 - p), 40.0 * k));
    }
    let fl = n * 0.2;
    Some(if d < 0.2 + fl {
        hex(255, 250, 200)
    } else if d < 0.55 + fl {
        hex(255, 170, 40)
    } else {
        hex(220, 60, 10)
    })
}

fn medikit(u: f64, v: f64) -> Option<u32> {
    if !(0.15..0.85).contains(&u) || !(0.5..1.0).contains(&v) {
        return None;
    }
    let cross = ((u - 0.5).abs() < 0.06 && (0.6..0.9).contains(&v)) || ((v - 0.75).abs() < 0.06 && (u - 0.5).abs() < 0.2);
    Some(if cross {
        hex(210, 20, 20)
    } else if u < 0.19 || u > 0.81 || v < 0.54 || v > 0.96 {
        hex(130, 130, 130)
    } else {
        hex(225, 225, 225)
    })
}

fn ammo_box(u: f64, v: f64) -> Option<u32> {
    if !(0.15..0.85).contains(&u) {
        return None;
    }
    if (0.62..1.0).contains(&v) {
        return Some(if v < 0.66 { hex(130, 130, 60) } else { hex(85, 85, 40) });
    }
    if (0.46..0.62).contains(&v) && ((u - 0.15) * 14.0) as i32 % 2 == 0 {
        return Some(if v > 0.57 { hex(200, 170, 60) } else { hex(180, 30, 20) });
    }
    None
}

/// (world size, bottom height, fullbright)
fn sprite_dims(e: &Ent) -> (f64, f64, bool) {
    match (e.kind, e.st) {
        (Kind::Imp | Kind::Demon, _) => (0.95, 0.0, false),
        (Kind::Fireball, St::Explode) => (0.7, 0.15, true),
        (Kind::Fireball, _) => (0.32, 0.38, true),
        _ => (0.45, 0.0, false),
    }
}

fn sprite_pixel(e: &Ent, u: f64, v: f64, time: f64) -> Option<u32> {
    let body = |u: f64, v: f64, attack: bool, step: bool| {
        if e.kind == Kind::Imp { imp(u, v, attack, step) } else { demon(u, v, attack, step) }
    };
    match e.kind {
        Kind::Imp | Kind::Demon => match e.st {
            St::Dead => corpse(u, v, e.kind == Kind::Demon),
            St::Dying => {
                let k = (e.timer / 0.5).clamp(0.0, 1.0);
                let s = 0.25 + 0.75 * k;
                let v2 = (v - (1.0 - s)) / s;
                if v2 < 0.0 {
                    return None;
                }
                body(u, v2, false, false).map(|c| mix(c, hex(170, 0, 0), 0.3 + 0.6 * (1.0 - k)))
            }
            St::Pain => body(u, v, false, false).map(|c| mix(c, 0xffffff, 0.45)),
            St::Attack => body(u, v, true, false),
            _ => body(u, v, false, (e.anim * 4.0) as i32 % 2 == 0),
        },
        Kind::Fireball => fireball(u, v, time, e.st == St::Explode, e.timer),
        Kind::Health => medikit(u, v),
        Kind::Ammo => ammo_box(u, v),
    }
}

fn gun_pixel(weapon: usize, u: f64, v: f64, flash: bool) -> Option<u32> {
    let skin = |k: f64| rgb(205.0 * k, 145.0 * k, 105.0 * k);
    let muzzle = |d: f64| mix(hex(255, 255, 220), hex(255, 130, 0), d);
    if weapon == 0 {
        if flash {
            let d = ell(u, v, 0.0, 0.1, 0.12, 0.1);
            if d < 1.0 {
                return Some(muzzle(d));
            }
        }
        let h = ell(u, v, 0.0, 0.92, 0.16, 0.26);
        if h < 1.0 {
            let knuckle = v > 0.75 && v < 0.79 && (u * 50.0).sin() > 0.3;
            return Some(skin(if knuckle { 0.7 } else { 1.0 - 0.35 * h }));
        }
        if u.abs() < 0.012 && v > 0.17 && v < 0.2 {
            return Some(hex(30, 30, 30));
        }
        if u.abs() < 0.045 && v > 0.2 && v < 0.55 {
            return Some(if u < -0.02 { hex(165, 165, 175) } else { hex(105, 105, 112) });
        }
        if u.abs() < 0.075 && v > 0.45 && v < 0.75 {
            return Some(if u < -0.05 { hex(120, 120, 128) } else { hex(70, 70, 76) });
        }
        None
    } else {
        if flash {
            let d = ell(u, v, 0.0, 0.02, 0.22, 0.14);
            if d < 1.0 {
                return Some(muzzle(d));
            }
        }
        let rh = ell(u, v, 0.13, 0.95, 0.15, 0.22);
        if rh < 1.0 {
            return Some(skin(1.0 - 0.35 * rh));
        }
        let lh = ell(u, v, -0.13, 0.5, 0.08, 0.1);
        if lh < 1.0 {
            return Some(skin(0.95 - 0.35 * lh));
        }
        if u.abs() < 0.13 && v > 0.42 && v < 0.58 {
            let g = 0.8 + 0.2 * (v * 90.0 + (u * 7.0).sin() * 3.0).sin();
            return Some(rgb(125.0 * g, 72.0 * g, 32.0 * g));
        }
        for bx in [-0.045, 0.045] {
            let du = (u - bx).abs();
            if du < 0.038 && v > 0.03 && v < 0.65 {
                if v < 0.06 && du < 0.022 {
                    return Some(hex(15, 15, 15));
                }
                let k = 1.0 - du / 0.038 * 0.6;
                return Some(rgb(95.0 * k, 95.0 * k, 102.0 * k));
            }
        }
        if u.abs() < 0.11 && v > 0.55 {
            return Some(hex(48, 48, 54));
        }
        None
    }
}

// ---------------------------------------------------------------- big pixel font (3x5)

const FONT: [&str; 26] = [
    "010101111101101", "110101110101110", "011100100100011", "110101101101110", "111100110100111",
    "111100110100100", "011100101101011", "101101111101101", "111010010010111", "001001001101010",
    "101101110101101", "100100100100111", "101111111101101", "110101101101101", "010101101101010",
    "110101110100100", "010101101110011", "110101110101101", "011100010001110", "111010010010010",
    "101101101101111", "101101101101010", "101101111111101", "101101010101101", "101101010010010",
    "111001010100111",
];

fn draw_text(fb: &mut [u32], w: usize, h: usize, text: &str, cy: usize, mut sc: usize, base: u32) {
    let n = text.chars().count();
    while sc > 1 && n * 4 * sc > w {
        sc -= 1;
    }
    let tw = n * 4 * sc - sc;
    if tw > w {
        return;
    }
    let x0 = (w - tw) / 2;
    let shadow = (sc / 3).max(1);
    for pass in 0..2 {
        let off = if pass == 0 { shadow } else { 0 };
        for (i, ch) in text.chars().enumerate() {
            if !ch.is_ascii_uppercase() {
                continue;
            }
            let gl = FONT[(ch as u8 - b'A') as usize].as_bytes();
            for r in 0..5 {
                let col = if pass == 0 { hex(20, 0, 0) } else { mix(base, hex(255, 220, 60), r as f64 / 6.0) };
                for c in 0..3 {
                    if gl[r * 3 + c] != b'1' {
                        continue;
                    }
                    for yy in 0..sc {
                        for xx in 0..sc {
                            let (x, y) = (x0 + i * 4 * sc + c * sc + xx + off, cy + r * sc + yy + off);
                            if x < w && y < h {
                                fb[y * w + x] = col;
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- rendering

fn render(g: &Game, tex: &[Vec<u32>], fb: &mut [u32], zbuf: &mut [f64], w: usize, h: usize) {
    let p = &g.p;
    let (dx, dy) = (p.a.cos(), p.a.sin());
    let (plx, ply) = (-dy * PLANE, dx * PLANE);
    let scale = w as f64 / (2.0 * PLANE);
    let horizon = h as f64 / 2.0;
    let light = 1.0 + p.flash * 3.0;
    let fog = |d: f64| ((-d * 0.11).exp() * light).clamp(0.06, 1.4);

    // floor & ceiling
    let (lx, ly, rx, ry) = (dx - plx, dy - ply, dx + plx, dy + ply);
    for y in 0..h {
        let yc = y as f64 + 0.5 - horizon;
        let row_d = 0.5 * scale / yc.abs();
        let f = fog(row_d);
        let t = if yc > 0.0 { &tex[4] } else { &tex[5] };
        let (sx, sy) = (row_d * (rx - lx) / w as f64, row_d * (ry - ly) / w as f64);
        let (mut fx, mut fy) = (p.x + row_d * lx + sx * 0.5, p.y + row_d * ly + sy * 0.5);
        for x in 0..w {
            let tx = ((fx * TEX as f64) as i64).rem_euclid(TEX as i64) as usize;
            let ty = ((fy * TEX as f64) as i64).rem_euclid(TEX as i64) as usize;
            fb[y * w + x] = shade(t[ty * TEX + tx], f);
            fx += sx;
            fy += sy;
        }
    }

    // walls
    for x in 0..w {
        let cam = 2.0 * (x as f64 + 0.5) / w as f64 - 1.0;
        let (rdx, rdy) = (dx + plx * cam, dy + ply * cam);
        let (t, c, side_x) = g.world.cast(p.x, p.y, rdx, rdy);
        let t = t.max(1e-4);
        zbuf[x] = t;
        let lh = scale / t;
        let top = horizon - lh * 0.5;
        let mut wx = if side_x { p.y + t * rdy } else { p.x + t * rdx };
        wx -= wx.floor();
        let mut tx = ((wx * TEX as f64) as usize).min(TEX - 1);
        if (side_x && rdx > 0.0) || (!side_x && rdy < 0.0) {
            tx = TEX - 1 - tx;
        }
        let f = fog(t) * if side_x { 1.0 } else { 0.75 };
        let wt = &tex[(c as usize - 1).min(3)];
        let y0 = top.max(0.0) as usize;
        let y1 = (horizon + lh * 0.5).min(h as f64).ceil() as usize;
        for y in y0..y1.min(h) {
            let ty = (((y as f64 + 0.5 - top) / lh * TEX as f64) as usize).min(TEX - 1);
            fb[y * w + x] = shade(wt[ty * TEX + tx], f);
        }
    }

    // sprites, far to near
    let inv = 1.0 / (plx * dy - dx * ply);
    let mut list: Vec<(f64, f64, usize)> = g
        .ents
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let (sx, sy) = (e.x - p.x, e.y - p.y);
            let depth = inv * (-ply * sx + plx * sy);
            (depth > 0.15).then(|| (depth, inv * (dy * sx - dx * sy), i))
        })
        .collect();
    list.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (depth, tx, i) in list {
        let e = &g.ents[i];
        let (size, z, bright) = sprite_dims(e);
        let scr_x = w as f64 / 2.0 * (1.0 + tx / depth);
        let sp = scale * size / depth;
        let bottom = horizon + (0.5 - z) * scale / depth;
        let top = bottom - sp;
        let left = scr_x - sp / 2.0;
        let f = if bright { 1.0 } else { fog(depth) };
        let (x0, x1) = (left.max(0.0) as usize, (left + sp).clamp(0.0, w as f64) as usize);
        let (y0, y1) = (top.max(0.0) as usize, bottom.clamp(0.0, h as f64) as usize);
        for x in x0..x1 {
            if depth >= zbuf[x] {
                continue;
            }
            let u = (x as f64 + 0.5 - left) / sp;
            for y in y0..y1 {
                let v = (y as f64 + 0.5 - top) / sp;
                if let Some(c) = sprite_pixel(e, u, v, g.time) {
                    fb[y * w + x] = shade(c, f);
                }
            }
        }
    }

    if g.mode != Mode::Title {
        draw_gun(g, fb, w, h, light);
        if g.minimap {
            draw_minimap(g, fb, w, h);
        }
    }

    // full-screen tints
    let p = &g.p;
    for c in fb.iter_mut() {
        let mut v = *c;
        match g.mode {
            Mode::Title => v = shade(v, 0.55),
            Mode::Dead => v = mix(shade(v, 0.7), hex(120, 0, 0), 0.4),
            _ => {}
        }
        if p.hurt > 0.0 {
            v = mix(v, hex(255, 0, 0), p.hurt * 0.45);
        }
        if p.bonus > 0.0 {
            v = mix(v, hex(255, 225, 100), p.bonus * 0.3);
        }
        *c = v;
    }

    let big = (h / 9).max(1);
    let small = (h / 30).max(1);
    match g.mode {
        Mode::Title => {
            draw_text(fb, w, h, "DOOM", h / 5, big, hex(200, 30, 0));
            draw_text(fb, w, h, "EN RUST DANS TON TERMINAL", h / 5 + big * 6 + 2, small, hex(180, 180, 180));
            if (g.time * 2.0) as i32 % 2 == 0 {
                draw_text(fb, w, h, "APPUIE SUR ENTREE", h * 3 / 4, small * 2, hex(200, 30, 0));
            }
        }
        Mode::Dead => {
            draw_text(fb, w, h, "TU ES MORT", h / 4, big, hex(200, 0, 0));
            draw_text(fb, w, h, "R POUR REJOUER", h / 4 + big * 6 + 2, small * 2, hex(200, 200, 200));
        }
        Mode::Won => {
            draw_text(fb, w, h, "NIVEAU TERMINE", h / 4, big, hex(200, 30, 0));
            draw_text(fb, w, h, "R POUR REJOUER", h / 4 + big * 6 + 2, small * 2, hex(200, 200, 200));
        }
        Mode::Playing => {
            // crosshair
            let (cx, cy) = (w / 2, h / 2);
            for (ox, oy) in [(-2i32, 0i32), (2, 0), (0, -2), (0, 2)] {
                let (x, y) = ((cx as i32 + ox) as usize, (cy as i32 + oy) as usize);
                if x < w && y < h {
                    fb[y * w + x] = hex(80, 255, 80);
                }
            }
        }
    }
}

fn draw_gun(g: &Game, fb: &mut [u32], w: usize, h: usize, light: f64) {
    let p = &g.p;
    let gh = h as f64 * 0.5;
    let bx = p.bob.sin() * gh * 0.07 * p.moving;
    let by = p.bob.cos().abs() * gh * 0.06 * p.moving;
    let recoil = if p.flash > 0.0 { gh * 0.08 } else { 0.0 };
    let top = h as f64 - gh * 0.92 + by + recoil;
    let cx = w as f64 / 2.0 + bx;
    let x0 = (cx - gh / 2.0).max(0.0) as usize;
    let x1 = (cx + gh / 2.0).clamp(0.0, w as f64) as usize;
    let flash = p.flash > 0.0;
    for y in top.max(0.0) as usize..h {
        let v = (y as f64 + 0.5 - top) / gh;
        for x in x0..x1 {
            let u = (x as f64 + 0.5 - cx) / gh;
            if let Some(c) = gun_pixel(p.weapon, u, v, flash) {
                fb[y * w + x] = shade(c, light.min(1.2));
            }
        }
    }
}

fn draw_minimap(g: &Game, fb: &mut [u32], w: usize, h: usize) {
    let cs = if w >= 200 && h >= 110 { 2 } else { 1 };
    let (mw, mh) = (g.world.w as usize * cs, g.world.h as usize * cs);
    if mw + 2 > w / 3 || mh + 2 > h / 2 {
        return;
    }
    let (ox, oy) = (w - mw - 1, 1);
    let mut put = |x: f64, y: f64, c: u32, a: f64| {
        let (px, py) = ((x * cs as f64) as usize, (y * cs as f64) as usize);
        for yy in 0..cs {
            for xx in 0..cs {
                let (fx, fy) = (ox + px + xx, oy + py + yy);
                if fx < w && fy < h && px + xx < mw && py + yy < mh {
                    let i = fy * w + fx;
                    fb[i] = mix(fb[i], c, a);
                }
            }
        }
    };
    let wall_col = [hex(150, 60, 40), hex(130, 130, 120), hex(80, 120, 170), hex(60, 140, 70)];
    for my in 0..g.world.h {
        for mx in 0..g.world.w {
            let c = g.world.cell(mx, my);
            let col = if c == 0 { hex(10, 10, 10) } else { wall_col[(c as usize - 1).min(3)] };
            put(mx as f64, my as f64, col, 0.75);
        }
    }
    for e in &g.ents {
        let col = match (e.kind, e.st) {
            (Kind::Imp | Kind::Demon, St::Dead | St::Dying) => hex(90, 20, 20),
            (Kind::Imp | Kind::Demon, _) => hex(255, 40, 40),
            (Kind::Fireball, _) => hex(255, 160, 0),
            _ => hex(40, 255, 80),
        };
        put(e.x, e.y, col, 1.0);
    }
    let p = &g.p;
    put(p.x + p.a.cos() * 1.2, p.y + p.a.sin() * 1.2, hex(160, 160, 40), 1.0);
    put(p.x, p.y, hex(255, 255, 60), 1.0);
}

// ---------------------------------------------------------------- terminal output

fn fg(out: &mut Vec<u8>, c: u32) {
    let (r, g, b) = split(c);
    let _ = write!(out, "\x1b[38;2;{};{};{}m", r as u8, g as u8, b as u8);
}
fn bg(out: &mut Vec<u8>, c: u32) {
    let (r, g, b) = split(c);
    let _ = write!(out, "\x1b[48;2;{};{};{}m", r as u8, g as u8, b as u8);
}

/// Emits only the cells that changed since last frame.
fn present(out: &mut Vec<u8>, fb: &[u32], prev: &mut [(u32, u32)], w: usize, rows: usize) {
    let (mut cur_fg, mut cur_bg) = (u32::MAX, u32::MAX);
    for r in 0..rows {
        let mut cursor_ok = false;
        for c in 0..w {
            let top = fb[2 * r * w + c] & 0xFCFCFC;
            let bot = fb[(2 * r + 1) * w + c] & 0xFCFCFC;
            let idx = r * w + c;
            if prev[idx] == (top, bot) {
                cursor_ok = false;
                continue;
            }
            prev[idx] = (top, bot);
            if !cursor_ok {
                let _ = write!(out, "\x1b[{};{}H", r + 1, c + 1);
                cursor_ok = true;
            }
            if top == bot {
                if bot != cur_bg {
                    bg(out, bot);
                    cur_bg = bot;
                }
                out.push(b' ');
            } else {
                if top != cur_fg {
                    fg(out, top);
                    cur_fg = top;
                }
                if bot != cur_bg {
                    bg(out, bot);
                    cur_bg = bot;
                }
                out.extend_from_slice("▀".as_bytes());
            }
        }
    }
}

fn hud_line(out: &mut Vec<u8>, row: usize, cols: usize, back: u32, segs: &[(String, u32)]) {
    let _ = write!(out, "\x1b[{};1H", row + 1);
    bg(out, back);
    let mut n = 0;
    for (s, c) in segs {
        fg(out, *c);
        for ch in s.chars() {
            if n >= cols {
                break;
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            n += 1;
        }
    }
    out.extend(std::iter::repeat_n(b' ', cols.saturating_sub(n)));
}

fn draw_hud(out: &mut Vec<u8>, g: &Game, cols: usize, row0: usize) {
    let p = &g.p;
    let (label, val, dim) = (hex(170, 170, 170), hex(230, 40, 30), hex(110, 110, 110));
    let hcol = match p.health {
        61.. => hex(60, 220, 60),
        31..=60 => hex(230, 200, 40),
        _ => hex(230, 40, 30),
    };
    let filled = ((p.health + 9) / 10) as usize;
    let bar = "█".repeat(filled) + &"░".repeat(10 - filled.min(10));
    let face = if p.health <= 0 {
        "(x_x)"
    } else if p.hurt > 0.2 {
        "(>_<)"
    } else if p.flash > 0.0 {
        "(ò_ó)"
    } else if p.health > 75 {
        "(•_•)"
    } else if p.health > 40 {
        "(o_O)"
    } else {
        "(;_;)"
    };
    let wpn = |i: usize, name: &str| {
        let c = if p.weapon == i { hex(255, 220, 60) } else { dim };
        (format!("[{}]{} ", i + 1, name), c)
    };
    let sep = || (" │ ".to_string(), dim);
    let line1 = vec![
        (" SANTÉ ".to_string(), label),
        (bar, hcol),
        (format!(" {:>3}%", p.health), hcol),
        sep(),
        ("BALLES ".to_string(), label),
        (format!("{:<3}", p.bullets), val),
        (" CARTOUCHES ".to_string(), label),
        (format!("{:<3}", p.shells), val),
        sep(),
        wpn(0, "PISTOLET"),
        wpn(1, "FUSIL"),
        sep(),
        ("TUÉS ".to_string(), label),
        (format!("{}/{}", g.kills, g.total), val),
        sep(),
        (face.to_string(), hex(255, 220, 60)),
    ];
    let back = hex(40, 38, 36);
    hud_line(out, row0, cols, back, &line1);
    let msg = if g.msg_t > 0.0 { format!(" > {}", g.msg) } else { String::new() };
    hud_line(out, row0 + 1, cols, back, &[(msg, hex(255, 220, 60))]);
    hud_line(
        out,
        row0 + 2,
        cols,
        hex(25, 24, 22),
        &[(
            " ZQSD/WASD bouger · ←→ tourner · ESPACE/F tirer · 1/2 arme · M carte · ESC quitter".to_string(),
            dim,
        )],
    );
    out.extend_from_slice(b"\x1b[0m");
}

// ---------------------------------------------------------------- main loop

fn restore(enhanced: bool) {
    let mut out = io::stdout();
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, crossterm::style::ResetColor, cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

fn run(stdout: &mut io::Stdout, enhanced: bool) -> io::Result<()> {
    let tex = make_textures();
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(42, |d| d.as_nanos() as u64);
    let mut game = Game::new(seed);
    let mut inp = Input { held: HashMap::new(), enhanced, pressed: Vec::new() };
    let (mut fb, mut zbuf, mut prev) = (Vec::new(), Vec::new(), Vec::new());
    let mut last_size = (0, 0);
    let mut out: Vec<u8> = Vec::with_capacity(1 << 20);
    let frame = Duration::from_secs_f64(1.0 / 35.0); // Doom ran at 35 Hz
    let mut last = Instant::now();

    loop {
        let start = Instant::now();
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(k) => {
                    let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
                    if k.kind == KeyEventKind::Press && (k.code == KeyCode::Esc || ctrl_c) {
                        return Ok(());
                    }
                    inp.on_key(k);
                }
                Event::Resize(..) => last_size = (0, 0),
                _ => {}
            }
        }
        let now = Instant::now();
        let dt = (now - last).as_secs_f64().min(0.1);
        last = now;
        game.update(dt, &mut inp);

        let (cols, rows) = terminal::size()?;
        let (cols, rows) = (cols as usize, rows as usize);
        out.clear();
        out.extend_from_slice(b"\x1b[?2026h"); // synchronized update
        if (cols, rows) != last_size {
            last_size = (cols, rows);
            out.extend_from_slice(b"\x1b[0m\x1b[2J");
            prev = vec![(u32::MAX, u32::MAX); cols * rows.saturating_sub(HUD_ROWS)];
        }
        if cols < 40 || rows < 12 {
            out.extend_from_slice("\x1b[0m\x1b[1;1HAgrandis le terminal (min 40x12)".as_bytes());
            last_size = (0, 0);
        } else {
            let vr = rows - HUD_ROWS;
            let (w, h) = (cols, vr * 2);
            fb.resize(w * h, 0);
            zbuf.resize(w, 0.0);
            render(&game, &tex, &mut fb, &mut zbuf, w, h);
            present(&mut out, &fb, &mut prev, w, vr);
            draw_hud(&mut out, &game, cols, vr);
        }
        out.extend_from_slice(b"\x1b[?2026l");
        stdout.write_all(&out)?;
        stdout.flush()?;

        if let Some(rest) = frame.checked_sub(start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

fn main() -> io::Result<()> {
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide, terminal::Clear(ClearType::All))?;
    let enhanced = matches!(terminal::supports_keyboard_enhancement(), Ok(true));
    if enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )?;
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore(enhanced);
        default_hook(info);
    }));
    let res = run(&mut stdout, enhanced);
    restore(enhanced);
    res
}
