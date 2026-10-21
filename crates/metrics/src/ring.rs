use std::collections::VecDeque;

/// Fixed-capacity FIFO that evicts the oldest item once full.
#[derive(Debug, Clone, PartialEq)]
pub struct Ring<T> {
    items: VecDeque<T>,
    capacity: usize,
}

impl<T> Ring<T> {
    /// Creates an empty ring. A capacity of 0 is raised to 1 so `push` always keeps
    /// the newest item.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            items: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Appends `item`, returning the evicted oldest item when the ring was full.
    pub fn push(&mut self, item: T) -> Option<T> {
        let evicted = (self.items.len() == self.capacity)
            .then(|| self.items.pop_front())
            .flatten();
        self.items.push_back(item);
        evicted
    }

    /// Maximum number of retained items.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of retained items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Reports whether nothing has been pushed since creation or [`Ring::clear`].
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Iterates oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.items.iter()
    }

    /// Returns the newest item.
    pub fn last(&self) -> Option<&T> {
        self.items.back()
    }

    /// Returns the oldest retained item.
    pub fn first(&self) -> Option<&T> {
        self.items.front()
    }

    /// Drops every item, keeping the capacity.
    pub fn clear(&mut self) {
        self.items.clear();
    }
}
