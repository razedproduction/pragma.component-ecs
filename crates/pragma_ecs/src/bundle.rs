//! Bundles: several components added to an entity at once, in one move to their final table
//! (DECISIONS #35).

use std::any::{TypeId, type_name};
use std::fmt;

use crate::EcsError;
use crate::archetype::Archetype;
use crate::component::{Column, ColumnData, Component};

mod sealed {
    /// Keeps [`Bundle`](super::Bundle) closed: its machinery is internal (DECISIONS #35).
    pub trait Sealed {}
}

use sealed::Sealed;

/// A set of components added to an entity together: a tuple of 0 to 12 components.
///
/// [`World::spawn_bundle`](crate::World::spawn_bundle) creates an entity directly in the table
/// of its final component set and [`World::insert_bundle`](crate::World::insert_bundle) moves an
/// entity once, instead of one table move per [`insert`](crate::World::insert);
/// [`Commands::spawn_bundle`](crate::Commands::spawn_bundle) and
/// [`Commands::insert_bundle`](crate::Commands::insert_bundle) record the same for the sync
/// point. A bundle may hold each component type once: `(A, A)` is
/// [`EcsError::DuplicateComponent`]. A one-component bundle is written `(value,)`.
///
/// A tuple is itself a [`Component`] (every `Send + Sync + 'static` type is), so
/// `world.insert(entity, (a, b))` attaches *one* component of type `(A, B)`; use
/// `insert_bundle` for two components. The trait is sealed: its methods are internal.
///
/// ```
/// use pragma_ecs::{Bundle, World};
///
/// #[derive(Debug, PartialEq)]
/// struct Position(f32, f32);
/// #[derive(Debug, PartialEq)]
/// struct Velocity(f32, f32);
/// struct Player;
///
/// fn assert_bundle<B: Bundle>(_: &B) {}
/// let bundle = (Position(0.0, 0.0), Velocity(1.0, 0.0), Player);
/// assert_bundle(&bundle);
///
/// let mut world = World::new();
/// let hero = world.spawn_bundle(bundle)?;
/// assert_eq!(world.get::<Velocity>(hero), Some(&Velocity(1.0, 0.0)));
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub trait Bundle: Sealed + Send + Sync + 'static {
    /// Number of components.
    #[doc(hidden)]
    const COUNT: usize;

    /// Appends the bundle's component types to `info`, in tuple order. Callers reserve room
    /// for [`COUNT`](Self::COUNT) entries first.
    #[doc(hidden)]
    fn describe(info: &mut BundleInfo);

    /// Writes every component into `row`.
    #[doc(hidden)]
    fn write(self, row: &mut BundleRow<'_>);
}

/// One component type of a bundle: its id, name and an empty column of it.
type Entry = (TypeId, &'static str, fn() -> Box<dyn Column>);

/// The component types of a bundle. Internal; visible only because [`Bundle`] names it.
#[doc(hidden)]
#[derive(Default)]
pub struct BundleInfo {
    components: Vec<Entry>,
}

impl fmt::Debug for BundleInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.components.iter().map(|(_, name, _)| name))
            .finish()
    }
}

impl BundleInfo {
    /// The component types of `B`, sorted by type id and checked for duplicates.
    ///
    /// `entities` goes into [`EcsError::OutOfMemory`] if the list cannot be allocated.
    pub(crate) fn of<B: Bundle>(entities: usize) -> Result<Self, EcsError> {
        let mut info = Self::default();
        info.components
            .try_reserve_exact(B::COUNT)
            .map_err(|_| EcsError::OutOfMemory { entities })?;
        B::describe(&mut info);
        info.components.sort_unstable_by_key(|(ty, _, _)| *ty);
        let duplicate = info.components.windows(2).find_map(|pair| match pair {
            [(a, name, _), (b, _, _)] if a == b => Some(*name),
            _ => None,
        });
        match duplicate {
            Some(component) => Err(EcsError::DuplicateComponent { component }),
            None => Ok(info),
        }
    }

    /// The component types with their column constructors, sorted (after [`of`](Self::of)).
    pub(crate) fn components(&self) -> impl Iterator<Item = (TypeId, fn() -> Box<dyn Column>)> {
        self.components.iter().map(|(ty, _, new)| (*ty, *new))
    }

    /// Adds the component type `T`. Does not allocate if room was reserved.
    pub(crate) fn push<T: Component>(&mut self) {
        self.components
            .push((TypeId::of::<T>(), type_name::<T>(), new_column::<T>));
    }
}

/// An empty column of `T`.
pub(crate) fn new_column<T: Component>() -> Box<dyn Column> {
    Box::new(ColumnData::<T>::default())
}

/// The row of a table a bundle is written into. Internal; visible only because [`Bundle`]
/// names it.
#[doc(hidden)]
pub struct BundleRow<'a> {
    table: &'a mut Archetype,
    row: usize,
    tick: u64,
}

impl fmt::Debug for BundleRow<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BundleRow")
            .field("row", &self.row)
            .finish_non_exhaustive()
    }
}

impl<'a> BundleRow<'a> {
    /// The row `row` of `table`, written at `tick`. Every column of the bundle's types must
    /// have room for one more row, or already hold the row.
    pub(crate) const fn new(table: &'a mut Archetype, row: usize, tick: u64) -> Self {
        Self { table, row, tick }
    }

    /// Writes `value`: replaces the component of the row (a change) or appends it (added).
    pub(crate) fn put<T: Component>(&mut self, value: T) {
        let written = self.table.put_component(self.row, value, self.tick);
        debug_assert!(written, "the table has a column of every bundle type");
    }
}

/// `COUNT` of a tuple: the number of its type parameters.
macro_rules! count {
    () => { 0 };
    ($head:ident $($tail:ident)*) => { 1 + count!($($tail)*) };
}

macro_rules! bundle {
    ($($name:ident $value:ident),*) => {
        impl<$($name: Component),*> Sealed for ($($name,)*) {}

        impl<$($name: Component),*> Bundle for ($($name,)*) {
            #[allow(clippy::arithmetic_side_effects, reason = "a constant of at most 12")]
            const COUNT: usize = count!($($name)*);

            #[allow(unused_variables)]
            fn describe(info: &mut BundleInfo) {
                $(info.push::<$name>();)*
            }

            #[allow(unused_variables)]
            fn write(self, row: &mut BundleRow<'_>) {
                let ($($value,)*) = self;
                $(row.put($value);)*
            }
        }
    };
}

bundle!();
bundle!(B0 b0);
bundle!(B0 b0, B1 b1);
bundle!(B0 b0, B1 b1, B2 b2);
bundle!(B0 b0, B1 b1, B2 b2, B3 b3);
bundle!(B0 b0, B1 b1, B2 b2, B3 b3, B4 b4);
bundle!(B0 b0, B1 b1, B2 b2, B3 b3, B4 b4, B5 b5);
bundle!(B0 b0, B1 b1, B2 b2, B3 b3, B4 b4, B5 b5, B6 b6);
bundle!(B0 b0, B1 b1, B2 b2, B3 b3, B4 b4, B5 b5, B6 b6, B7 b7);
bundle!(B0 b0, B1 b1, B2 b2, B3 b3, B4 b4, B5 b5, B6 b6, B7 b7, B8 b8);
bundle!(B0 b0, B1 b1, B2 b2, B3 b3, B4 b4, B5 b5, B6 b6, B7 b7, B8 b8, B9 b9);
bundle!(B0 b0, B1 b1, B2 b2, B3 b3, B4 b4, B5 b5, B6 b6, B7 b7, B8 b8, B9 b9, B10 b10);
bundle!(B0 b0, B1 b1, B2 b2, B3 b3, B4 b4, B5 b5, B6 b6, B7 b7, B8 b8, B9 b9, B10 b10, B11 b11);

#[cfg(test)]
mod tests {
    use super::*;

    struct A;
    struct B;

    #[test]
    fn types_are_sorted_and_duplicates_rejected() {
        let info = BundleInfo::of::<(B, A, u8)>(0).unwrap();
        let mut expected = [TypeId::of::<A>(), TypeId::of::<B>(), TypeId::of::<u8>()];
        expected.sort_unstable();
        assert_eq!(
            info.components().map(|(ty, _)| ty).collect::<Vec<_>>(),
            expected
        );
        assert!(format!("{info:?}").contains("u8"));
        assert_eq!(<(A, B, u8)>::COUNT, 3);
        assert_eq!(<()>::COUNT, 0);

        let duplicate = BundleInfo::of::<(A, u8, A)>(0).map(|_| ());
        assert_eq!(
            duplicate,
            Err(EcsError::DuplicateComponent {
                component: type_name::<A>()
            })
        );
    }
}
