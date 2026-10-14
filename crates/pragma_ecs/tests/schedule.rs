//! Schedules: systems on the `pragma_core` frame graph (DECISIONS #17–#21).
//!
//! The property tests generate random sets of systems over a few component and resource types,
//! with random explicit orders, and check that a schedule builds exactly when every pair of
//! conflicting systems is ordered, and that a frame changes the world exactly as running the
//! systems one by one in the schedule's order does, on 1 and 4 workers.
// Test code: a panic is a test failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
use pragma_ecs::{
    AccessKind, EcsError, Entity, Query, Res, ResMut, Schedule, ScheduleBuilder, SystemConfig,
    View, World,
};
use proptest::prelude::*;

/// A component of kind `K`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct C<const K: usize>(u64);

/// A resource of kind `K`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct R<const K: usize>(u64);

fn pools() -> &'static [ThreadPool; 2] {
    static POOLS: OnceLock<[ThreadPool; 2]> = OnceLock::new();
    POOLS.get_or_init(|| {
        [1, 4].map(|workers| ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap())
    })
}

// The operations of the generated systems. They do not commute with each other, so running two
// conflicting systems in the wrong order changes the result.

fn mix<const I: usize, const J: usize>(a: &mut C<I>, b: &C<J>) {
    a.0 =
        a.0.wrapping_mul(2 * I as u64 + 3)
            .wrapping_add(b.0 ^ J as u64);
}

fn bump<const I: usize>(a: &mut C<I>) {
    a.0 = a.0.wrapping_mul(7).wrapping_add(1 + I as u64);
}

fn absorb<const K: usize>(r: &mut R<K>, sum: u64) {
    r.0 = r.0.wrapping_mul(31).wrapping_add(sum);
}

fn shift<const I: usize, const K: usize>(a: &mut C<I>, r: &R<K>) {
    a.0 = a.0.wrapping_add(r.0).rotate_left(5);
}

// The generated systems.

fn mix_system<const I: usize, const J: usize>(
    mut rows: View<(&mut C<I>, &C<J>)>,
) -> Result<(), EcsError> {
    rows.iter_mut()?.for_each(|(a, b)| mix(a, b));
    Ok(())
}

fn bump_system<const I: usize>(mut rows: View<&mut C<I>>) -> Result<(), EcsError> {
    rows.par_for_each_mut(Grain::Fixed(3), bump)
}

fn absorb_system<const I: usize, const K: usize>(
    mut rows: View<&C<I>>,
    mut r: ResMut<R<K>>,
) -> Result<(), EcsError> {
    let sum = rows.iter()?.fold(0_u64, |s, a| s.wrapping_add(a.0));
    absorb(&mut r, sum);
    Ok(())
}

fn shift_system<const I: usize, const K: usize>(
    mut rows: View<(&mut C<I>, Option<&u8>)>,
    r: Res<R<K>>,
) -> Result<(), EcsError> {
    rows.iter_mut()?.for_each(|(a, _)| shift(a, &r));
    Ok(())
}

// The same operations applied directly to the world, one system at a time.

fn mix_world<const I: usize, const J: usize>(world: &mut World) {
    let mut query = Query::<(&mut C<I>, &C<J>)>::new().unwrap();
    query.iter_mut(world).unwrap().for_each(|(a, b)| mix(a, b));
}

fn bump_world<const I: usize>(world: &mut World) {
    let mut query = Query::<&mut C<I>>::new().unwrap();
    query.iter_mut(world).unwrap().for_each(bump);
}

fn absorb_world<const I: usize, const K: usize>(world: &mut World) {
    let mut query = Query::<&C<I>>::new().unwrap();
    let sum = query
        .iter(world)
        .unwrap()
        .fold(0_u64, |s, a| s.wrapping_add(a.0));
    absorb(world.resource_mut::<R<K>>().unwrap(), sum);
}

fn shift_world<const I: usize, const K: usize>(world: &mut World) {
    let r = *world.resource::<R<K>>().unwrap();
    let mut query = Query::<&mut C<I>>::new().unwrap();
    query.iter_mut(world).unwrap().for_each(|a| shift(a, &r));
}

/// One generated system. Components 0..3, resources 0..2.
#[derive(Clone, Copy, Debug)]
enum Spec {
    /// Writes component `.0`, reads component `.1` (different).
    Mix(usize, usize),
    /// Writes component `.0`, in parallel.
    Bump(usize),
    /// Reads component `.0`, writes resource `.1`.
    Absorb(usize, usize),
    /// Writes component `.0`, reads resource `.1` (and the never present `u8`).
    Shift(usize, usize),
}

impl Spec {
    /// (component reads, component writes, resource reads, resource writes).
    fn access(self) -> (Vec<usize>, Vec<usize>, Vec<usize>, Vec<usize>) {
        match self {
            Self::Mix(i, j) => (vec![j], vec![i], vec![], vec![]),
            Self::Bump(i) => (vec![], vec![i], vec![], vec![]),
            Self::Absorb(i, k) => (vec![i], vec![], vec![], vec![k]),
            Self::Shift(i, k) => (vec![], vec![i], vec![k], vec![]),
        }
    }

    fn conflicts(self, other: Self) -> bool {
        let (cr, cw, rr, rw) = self.access();
        let (or, ow, orr, orw) = other.access();
        let clash = |writes: &[usize], reads: &[usize], their_w: &[usize]| {
            writes
                .iter()
                .any(|t| reads.contains(t) || their_w.contains(t))
        };
        clash(&cw, &or, &ow)
            || clash(&ow, &cr, &cw)
            || clash(&rw, &orr, &orw)
            || clash(&orw, &rr, &rw)
    }
}

/// Calls a generic function for the concrete types of a [`Spec`].
trait Visit {
    type Out;
    fn mix<const I: usize, const J: usize>(self) -> Self::Out;
    fn bump<const I: usize>(self) -> Self::Out;
    fn absorb<const I: usize, const K: usize>(self) -> Self::Out;
    fn shift<const I: usize, const K: usize>(self) -> Self::Out;
}

fn dispatch<V: Visit>(spec: Spec, v: V) -> V::Out {
    match spec {
        Spec::Mix(0, 1) => v.mix::<0, 1>(),
        Spec::Mix(0, _) => v.mix::<0, 2>(),
        Spec::Mix(1, 0) => v.mix::<1, 0>(),
        Spec::Mix(1, _) => v.mix::<1, 2>(),
        Spec::Mix(_, 0) => v.mix::<2, 0>(),
        Spec::Mix(_, _) => v.mix::<2, 1>(),
        Spec::Bump(0) => v.bump::<0>(),
        Spec::Bump(1) => v.bump::<1>(),
        Spec::Bump(_) => v.bump::<2>(),
        Spec::Absorb(0, 0) => v.absorb::<0, 0>(),
        Spec::Absorb(0, _) => v.absorb::<0, 1>(),
        Spec::Absorb(1, 0) => v.absorb::<1, 0>(),
        Spec::Absorb(1, _) => v.absorb::<1, 1>(),
        Spec::Absorb(_, 0) => v.absorb::<2, 0>(),
        Spec::Absorb(_, _) => v.absorb::<2, 1>(),
        Spec::Shift(0, 0) => v.shift::<0, 0>(),
        Spec::Shift(0, _) => v.shift::<0, 1>(),
        Spec::Shift(1, 0) => v.shift::<1, 0>(),
        Spec::Shift(1, _) => v.shift::<1, 1>(),
        Spec::Shift(_, 0) => v.shift::<2, 0>(),
        Spec::Shift(_, _) => v.shift::<2, 1>(),
    }
}

/// Adds the system of a spec to a builder.
struct Add<'a> {
    builder: &'a mut ScheduleBuilder,
    name: String,
    after: Vec<String>,
}

impl Add<'_> {
    fn config(after: Vec<String>) -> impl FnOnce(SystemConfig) -> SystemConfig {
        move |c| after.into_iter().fold(c, SystemConfig::after)
    }
}

impl Visit for Add<'_> {
    type Out = ();
    fn mix<const I: usize, const J: usize>(self) {
        let config = Self::config(self.after);
        self.builder
            .add_system_with(self.name, mix_system::<I, J>, config)
            .unwrap();
    }
    fn bump<const I: usize>(self) {
        let config = Self::config(self.after);
        self.builder
            .add_system_with(self.name, bump_system::<I>, config)
            .unwrap();
    }
    fn absorb<const I: usize, const K: usize>(self) {
        let config = Self::config(self.after);
        self.builder
            .add_system_with(self.name, absorb_system::<I, K>, config)
            .unwrap();
    }
    fn shift<const I: usize, const K: usize>(self) {
        let config = Self::config(self.after);
        self.builder
            .add_system_with(self.name, shift_system::<I, K>, config)
            .unwrap();
    }
}

/// Applies the operation of a spec directly to a world.
struct Apply<'a>(&'a mut World);

impl Visit for Apply<'_> {
    type Out = ();
    fn mix<const I: usize, const J: usize>(self) {
        mix_world::<I, J>(self.0);
    }
    fn bump<const I: usize>(self) {
        bump_world::<I>(self.0);
    }
    fn absorb<const I: usize, const K: usize>(self) {
        absorb_world::<I, K>(self.0);
    }
    fn shift<const I: usize, const K: usize>(self) {
        shift_world::<I, K>(self.0);
    }
}

fn spec() -> impl Strategy<Value = Spec> {
    prop_oneof![
        (0..3_usize, 1..3_usize).prop_map(|(i, d)| Spec::Mix(i, (i + d) % 3)),
        (0..3_usize).prop_map(Spec::Bump),
        (0..3_usize, 0..2_usize).prop_map(|(i, k)| Spec::Absorb(i, k)),
        (0..3_usize, 0..2_usize).prop_map(|(i, k)| Spec::Shift(i, k)),
    ]
}

/// Entities with random subsets of the three components (each `Some` is present).
fn entity() -> impl Strategy<Value = [Option<u64>; 3]> {
    any::<[Option<u64>; 3]>()
}

fn world(entities: &[[Option<u64>; 3]], resources: [u64; 2]) -> (World, Vec<Entity>) {
    let mut world = World::new();
    let ids = entities
        .iter()
        .map(|values| {
            let e = world.spawn().unwrap();
            if let Some(v) = values[0] {
                world.insert(e, C::<0>(v)).unwrap();
            }
            if let Some(v) = values[1] {
                world.insert(e, C::<1>(v)).unwrap();
            }
            if let Some(v) = values[2] {
                world.insert(e, C::<2>(v)).unwrap();
            }
            e
        })
        .collect();
    world.insert_resource(R::<0>(resources[0]));
    world.insert_resource(R::<1>(resources[1]));
    (world, ids)
}

type State = (Vec<[Option<u64>; 3]>, [u64; 2]);

fn state(world: &World, ids: &[Entity]) -> State {
    let components = ids
        .iter()
        .map(|&e| {
            [
                world.get::<C<0>>(e).map(|c| c.0),
                world.get::<C<1>>(e).map(|c| c.0),
                world.get::<C<2>>(e).map(|c| c.0),
            ]
        })
        .collect();
    let resources = [
        world.resource::<R<0>>().unwrap().0,
        world.resource::<R<1>>().unwrap().0,
    ];
    (components, resources)
}

/// Whether `to` is reachable from `from` over `edges` (earlier → later system indices).
fn reaches(edges: &[(usize, usize)], from: usize, to: usize) -> bool {
    let mut stack = vec![from];
    let mut seen = vec![from];
    while let Some(u) = stack.pop() {
        for &(a, b) in edges {
            if a == u && !seen.contains(&b) {
                if b == to {
                    return true;
                }
                seen.push(b);
                stack.push(b);
            }
        }
    }
    false
}

fn build(specs: &[Spec], edges: &[(usize, usize)], ordered: bool) -> Result<Schedule, EcsError> {
    let mut builder = if ordered {
        Schedule::builder().ordered()
    } else {
        Schedule::builder()
    };
    for (j, &spec) in specs.iter().enumerate() {
        let after = edges
            .iter()
            .filter(|&&(_, b)| b == j)
            .map(|&(a, _)| format!("s{a}"))
            .collect();
        dispatch(
            spec,
            Add {
                builder: &mut builder,
                name: format!("s{j}"),
                after,
            },
        );
    }
    builder.build()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Invariant: a schedule builds iff every pair of conflicting systems is ordered by explicit
    /// edges (transitively); the `ordered` mode always builds.
    #[test]
    fn builds_iff_no_unordered_conflict(
        specs in proptest::collection::vec(spec(), 1..8),
        raw_edges in proptest::collection::vec((0..8_usize, 0..8_usize), 0..10),
    ) {
        let n = specs.len();
        let edges: Vec<(usize, usize)> = raw_edges
            .into_iter()
            .map(|(a, b)| (a % n, b % n))
            .filter(|(a, b)| a < b)
            .collect();
        let unordered = (0..n).flat_map(|i| (i + 1..n).map(move |j| (i, j))).find(|&(i, j)| {
            specs[i].conflicts(specs[j]) && !reaches(&edges, i, j)
        });
        match (build(&specs, &edges, false), unordered) {
            (Ok(_), None) => {}
            (Err(EcsError::AmbiguousOrder { first, second, .. }), Some(_)) => {
                let index = |name: &str| name[1..].parse::<usize>().unwrap();
                let (i, j) = (index(&first), index(&second));
                prop_assert!(specs[i].conflicts(specs[j]));
                prop_assert!(!reaches(&edges, i, j) && !reaches(&edges, j, i));
            }
            (other, expected) => prop_assert!(false, "{other:?}, unordered pair {expected:?}"),
        }
        prop_assert!(build(&specs, &edges, true).is_ok());
    }

    /// Invariant: a frame changes the world exactly as the systems applied one by one in the
    /// schedule's order, on 1 and 4 workers, for several frames.
    #[test]
    fn frames_equal_a_sequential_run(
        specs in proptest::collection::vec(spec(), 1..7),
        entities in proptest::collection::vec(entity(), 0..60),
        resources in any::<[u64; 2]>(),
        frames in 1..4_usize,
    ) {
        let schedule = build(&specs, &[], true).unwrap();
        let order: Vec<usize> = schedule.order().map(|name| name[1..].parse().unwrap()).collect();
        let (mut expected, ids) = world(&entities, resources);
        for _ in 0..frames {
            for &i in &order {
                dispatch(specs[i], Apply(&mut expected));
            }
        }
        let expected = state(&expected, &ids);

        for pool in pools() {
            let mut schedule = build(&specs, &[], true).unwrap();
            let (mut actual, same) = world(&entities, resources);
            prop_assert_eq!(&same, &ids);
            for _ in 0..frames {
                schedule.run(&mut actual, pool).unwrap();
            }
            prop_assert_eq!(&state(&actual, &ids), &expected);
        }
    }
}

// Behaviour of single schedules.

#[derive(Debug, PartialEq)]
struct Position(i32);
#[derive(Debug, PartialEq)]
struct Velocity(i32);
struct Health(i32);
struct Log(Vec<&'static str>);

/// How many of the two meeting systems are running.
static ARRIVED: AtomicUsize = AtomicUsize::new(0);

/// Waits up to two seconds for the other meeting system; `true` if they ran at the same time.
fn meet() -> bool {
    ARRIVED.fetch_add(1, Ordering::SeqCst);
    let start = Instant::now();
    while ARRIVED.load(Ordering::SeqCst) < 2 {
        if start.elapsed() > Duration::from_secs(2) {
            return false;
        }
        std::hint::spin_loop();
    }
    true
}

fn meet_positions(_: View<&mut Position>, mut met: ResMut<R<0>>) {
    met.0 = u64::from(meet());
}

fn meet_healths(_: View<&mut Health>, mut met: ResMut<R<1>>) {
    met.0 = u64::from(meet());
}

#[test]
fn systems_with_disjoint_access_run_at_the_same_time() {
    let mut world = World::new();
    world.insert_resource(R::<0>(0));
    world.insert_resource(R::<1>(0));
    let mut builder = Schedule::builder();
    builder
        .add_system("positions", meet_positions)
        .unwrap()
        .add_system("healths", meet_healths)
        .unwrap();
    let mut schedule = builder.build().unwrap();
    assert_eq!(schedule.edges().count(), 0);
    schedule.run(&mut world, &pools()[1]).unwrap();
    assert_eq!(world.resource::<R<0>>().unwrap().0, 1);
    assert_eq!(world.resource::<R<1>>().unwrap().0, 1);
}

fn integrate(mut bodies: View<(&mut Position, &Velocity)>) -> Result<(), EcsError> {
    bodies
        .iter_mut()?
        .for_each(|(position, velocity)| position.0 += velocity.0);
    Ok(())
}

fn hurt(mut healths: View<&mut Health>, mut log: ResMut<Log>) -> Result<(), String> {
    for health in healths.iter_mut().map_err(|e| e.to_string())? {
        health.0 -= 10;
        if health.0 < 0 {
            return Err(format!("health {} below zero", health.0));
        }
    }
    log.0.push("hurt");
    Ok(())
}

fn report(mut log: ResMut<Log>) {
    log.0.push("report");
}

fn crash(_: View<&Velocity>) {
    panic!("crashed");
}

#[test]
fn failures_are_typed_skip_dependents_and_leave_the_world_whole() {
    let mut world = World::new();
    let e = world.spawn().unwrap();
    world.insert(e, Position(0)).unwrap();
    world.insert(e, Velocity(2)).unwrap();
    world.insert(e, Health(15)).unwrap();
    world.insert_resource(Log(Vec::new()));

    let mut builder = Schedule::builder();
    builder.add_system("integrate", integrate).unwrap();
    builder.add_system("hurt", hurt).unwrap();
    builder
        .add_system_with("report", report, |c| c.after("hurt"))
        .unwrap();
    let mut schedule = builder.build().unwrap();
    assert!(format!("{schedule:?}").starts_with("Schedule { order: ["));
    let pool = &pools()[1];

    schedule.run(&mut world, pool).unwrap();
    assert_eq!(world.resource::<Log>().unwrap().0, ["hurt", "report"]);

    // Health goes below zero: "hurt" fails, "report" after it is skipped, "integrate" runs.
    match schedule.run(&mut world, pool) {
        Err(EcsError::SystemFailed {
            system,
            message,
            source,
            ..
        }) => {
            assert_eq!(system, "hurt");
            assert_eq!(message, "health -5 below zero");
            assert!(source.is_none());
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(world.resource::<Log>().unwrap().0, ["hurt", "report"]);
    assert_eq!(world.get::<Position>(e), Some(&Position(4)));
    // Every lent column came back.
    assert_eq!(world.get::<Velocity>(e), Some(&Velocity(2)));
    assert_eq!(world.get::<Health>(e).map(|h| h.0), Some(-5));

    // A panicking system is caught; the next frame still runs.
    let mut builder = Schedule::builder();
    builder.add_system("crash", crash).unwrap();
    builder.add_system("integrate", integrate).unwrap();
    let mut crashing = builder.build().unwrap();
    assert!(matches!(
        crashing.run(&mut world, pool),
        Err(EcsError::SystemPanicked { ref system, ref message, .. })
            if system == "crash" && message == "crashed"
    ));
    assert_eq!(world.get::<Position>(e), Some(&Position(6)));
    assert!(crashing.run(&mut world, pool).is_err());
    assert_eq!(world.get::<Position>(e), Some(&Position(8)));
}

fn lookup(mut rows: View<(Entity, &Position)>, mut found: ResMut<R<0>>) -> Result<(), EcsError> {
    found.0 = rows.iter()?.count() as u64;
    Ok(())
}

#[test]
fn errors_of_building_and_missing_resources_are_typed() {
    // Two systems under one name, an unknown dependency, a cycle.
    let mut builder = Schedule::builder();
    builder.add_system("a", report).unwrap();
    builder.add_system("a", integrate).unwrap();
    assert!(matches!(builder.build(), Err(EcsError::Schedule { .. })));
    let mut builder = Schedule::builder();
    builder
        .add_system_with("a", integrate, |c| c.after("b").before("b"))
        .unwrap();
    builder.add_system("b", report).unwrap();
    assert!(matches!(builder.build(), Err(EcsError::Schedule { .. })));

    // A conflict on a resource is named by the resource.
    let mut builder = Schedule::builder();
    builder.add_system("hurt", hurt).unwrap();
    builder.add_system("report", report).unwrap();
    match builder.build() {
        Err(EcsError::AmbiguousOrder {
            first,
            second,
            data,
            ..
        }) => {
            assert_eq!((first.as_str(), second.as_str()), ("hurt", "report"));
            assert_eq!(data, std::any::type_name::<Log>());
        }
        other => panic!("unexpected {other:?}"),
    }

    // A missing resource stops the frame before any system runs.
    let mut builder = Schedule::builder();
    builder.add_system("integrate", integrate).unwrap();
    builder
        .add_system_with("lookup", lookup, |c| c.after("integrate"))
        .unwrap();
    let mut schedule = builder.build().unwrap();
    assert!(!schedule.is_empty());
    let mut world = World::new();
    let e = world.spawn().unwrap();
    world.insert(e, Position(1)).unwrap();
    world.insert(e, Velocity(1)).unwrap();
    assert!(matches!(
        schedule.run(&mut world, &pools()[0]),
        Err(EcsError::MissingResource { ref resource, .. }) if resource.ends_with("R<0>")
    ));
    assert_eq!(world.get::<Position>(e), Some(&Position(1)));
    world.insert_resource(R::<0>(0));
    schedule.run(&mut world, &pools()[0]).unwrap();
    assert_eq!(world.get::<Position>(e), Some(&Position(2)));
    assert_eq!(world.resource::<R<0>>().unwrap().0, 1);
}

#[test]
fn tables_created_between_frames_and_other_worlds_are_seen() {
    let mut builder = Schedule::builder();
    builder.add_system("integrate", integrate).unwrap();
    let mut schedule = builder.build().unwrap();
    let (_, access) = schedule.systems().next().unwrap();
    assert_eq!(access.kind_of::<Position>(), Some(AccessKind::Write));
    assert_eq!(access.kind_of::<Velocity>(), Some(AccessKind::Read));
    let pool = &pools()[1];

    let mut world = World::new();
    let a = world.spawn().unwrap();
    world.insert(a, Position(0)).unwrap();
    world.insert(a, Velocity(1)).unwrap();
    schedule.run(&mut world, pool).unwrap();

    // A new table {Position, Velocity, Health} and a table without velocity.
    let b = world.spawn().unwrap();
    world.insert(b, Health(1)).unwrap();
    world.insert(b, Velocity(10)).unwrap();
    world.insert(b, Position(0)).unwrap();
    let c = world.spawn().unwrap();
    world.insert(c, Position(0)).unwrap();
    schedule.run(&mut world, pool).unwrap();
    assert_eq!(world.get::<Position>(a), Some(&Position(2)));
    assert_eq!(world.get::<Position>(b), Some(&Position(10)));
    assert_eq!(world.get::<Position>(c), Some(&Position(0)));

    // The same schedule over another world.
    let mut other = World::new();
    let d = other.spawn().unwrap();
    other.insert(d, Velocity(5)).unwrap();
    other.insert(d, Position(0)).unwrap();
    schedule.run(&mut other, pool).unwrap();
    assert_eq!(other.get::<Position>(d), Some(&Position(5)));
    schedule.run(&mut world, pool).unwrap();
    assert_eq!(world.get::<Position>(b), Some(&Position(20)));
}
