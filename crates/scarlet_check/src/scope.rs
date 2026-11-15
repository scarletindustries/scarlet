//! Which local each name means where. A function body is a frame, and each
//! block inside it is a level of that frame. Frame 0 is the module's own
//! scope.

use scarlet_types::name::Name;

use crate::typed::BinderId;

#[derive(Default)]
struct Frame {
    blocks: Vec<Vec<(Name, BinderId)>>,
    /// The enclosing frames' locals this frame's body reads, in first-use
    /// order.
    captures: Vec<BinderId>,
    /// The name a lambda bound by `x = fn ..` calls itself by.
    self_name: Option<(Name, BinderId)>,
}

impl Frame {
    fn lookup(&self, name: &str) -> Option<BinderId> {
        self.blocks.iter().rev().find_map(|b| {
            b.iter()
                .rev()
                .find(|(n, _)| &**n == name)
                .map(|&(_, id)| id)
        })
    }
}

pub(crate) struct Scope {
    /// Frame 0, which is always there.
    module: Frame,
    /// The function bodies open inside it, innermost last.
    inner: Vec<Frame>,
}

/// Where a local was found.
pub(crate) struct Found {
    pub binder: BinderId,
    /// The frame that binds it.
    pub frame: usize,
}

impl Scope {
    pub fn new() -> Self {
        Scope {
            module: Frame {
                blocks: vec![Vec::new()],
                ..Frame::default()
            },
            inner: Vec::new(),
        }
    }

    /// The innermost frame's index.
    pub fn depth(&self) -> usize {
        self.inner.len()
    }

    /// Whether a binding made now is one of the module's own statements: no
    /// function is open, and no block inside a statement either.
    pub fn at_module_scope(&self) -> bool {
        self.inner.is_empty() && self.module.blocks.len() == 1
    }

    fn frames(&self) -> impl Iterator<Item = &Frame> {
        std::iter::once(&self.module).chain(&self.inner)
    }

    fn current(&mut self) -> &mut Frame {
        match self.inner.last_mut() {
            Some(f) => f,
            None => &mut self.module,
        }
    }

    pub fn open_block(&mut self) {
        self.current().blocks.push(Vec::new());
    }

    pub fn close_block(&mut self) {
        self.current().blocks.pop();
    }

    pub fn insert(&mut self, name: Name, id: BinderId) {
        if let Some(b) = self.current().blocks.last_mut() {
            b.push((name, id));
        }
    }

    pub fn open_frame(&mut self, self_name: Option<(Name, BinderId)>) {
        self.inner.push(Frame {
            blocks: vec![Vec::new()],
            captures: Vec::new(),
            self_name,
        });
    }

    /// Close the innermost frame, giving back what it captured.
    pub fn close_frame(&mut self) -> Vec<BinderId> {
        self.inner.pop().map(|f| f.captures).unwrap_or_default()
    }

    /// The local `name` means here, innermost first: a frame's own blocks,
    /// then the name the frame's lambda calls itself by, then the frames
    /// around it.
    pub fn lookup(&self, name: &str) -> Option<Found> {
        let innermost_first = self
            .inner
            .iter()
            .enumerate()
            .rev()
            .map(|(i, f)| (i + 1, f))
            .chain(std::iter::once((0, &self.module)));
        for (i, f) in innermost_first {
            if let Some(binder) = f.lookup(name) {
                return Some(Found { binder, frame: i });
            }
            if let Some((n, b)) = &f.self_name
                && &**n == name
            {
                return Some(Found {
                    binder: *b,
                    frame: i,
                });
            }
        }
        None
    }

    /// Record that the frames inside `bound_in` read `binder`, so each lambda
    /// on the way captures it.
    pub fn capture(&mut self, binder: BinderId, bound_in: usize) {
        for f in self.inner.iter_mut().skip(bound_in) {
            if !f.captures.contains(&binder) {
                f.captures.push(binder);
            }
        }
    }

    /// Every local name in scope, for "did you mean".
    pub fn names(&self) -> impl Iterator<Item = &Name> {
        self.frames()
            .flat_map(|f| f.blocks.iter().flatten().map(|(n, _)| n))
    }
}
