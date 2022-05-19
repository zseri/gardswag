#![forbid(
    trivial_casts,
    unconditional_recursion,
    unsafe_code,
    unused_must_use,
    clippy::as_conversions,
    clippy::cast_ptr_alignment
)]
#![deny(unused_variables)]
#![no_std]

use core::fmt;

pub struct VarStack<'a, V> {
    pub parent: Option<&'a VarStack<'a, V>>,
    pub name: &'a str,
    pub value: V,
}

impl<V: fmt::Debug> fmt::Debug for VarStack<'_, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<'a, V> VarStack<'a, V> {
    pub fn find(&self, name: &str) -> Option<&V> {
        let mut this = self;
        while this.name != name {
            this = *this.parent.as_ref()?;
        }
        Some(&this.value)
    }
    pub fn iter(&'a self) -> Iter<'a, V> {
        Iter { inner: Some(self) }
    }
}

#[derive(Debug)]
pub struct Iter<'a, V> {
    inner: Option<&'a VarStack<'a, V>>,
}

impl<'a, V> Iterator for Iter<'a, V> {
    type Item = (&'a str, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let inner = self.inner.take()?;
        self.inner = inner.parent;
        Some((inner.name, &inner.value))
    }
}
