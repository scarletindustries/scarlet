use indexmap::IndexMap;

use super::compiler::Compiler;
use crate::types::{InferType, mono};

// Builtins are VM intrinsics that provide low-level functionality beyond
// the prelude. These will eventually move behind standard library imports
// once the module system is fully implemented.

impl Compiler {
    pub(super) fn register_builtins(&mut self) {
        self.env.register_struct("Socket", vec![], IndexMap::new());

        let socket = self.engine.icon("Socket", vec![]);
        let i = self.engine.icon_int();
        let s = self.engine.icon_string();
        let n = self.engine.icon_none();

        self.env.define(
            "__stack_depth__",
            mono(InferType::IFun {
                params: vec![],
                ret: Box::new(i.clone()),
                err: None,
            }),
        );

        self.env.define(
            "read_file",
            mono(InferType::IFun {
                params: vec![s.clone()],
                ret: Box::new(s.clone()),
                err: Some(Box::new(s.clone())),
            }),
        );
        self.env.define(
            "write_file",
            mono(InferType::IFun {
                params: vec![s.clone(), s.clone()],
                ret: Box::new(n.clone()),
                err: Some(Box::new(s.clone())),
            }),
        );

        self.env.define(
            "str_split",
            mono(InferType::IFun {
                params: vec![s.clone(), s.clone()],
                ret: Box::new(self.engine.icon_array(s.clone())),
                err: None,
            }),
        );

        self.env.define(
            "tcp_listen",
            mono(InferType::IFun {
                params: vec![i],
                ret: Box::new(socket.clone()),
                err: Some(Box::new(s.clone())),
            }),
        );
        self.env.define(
            "tcp_accept",
            mono(InferType::IFun {
                params: vec![socket.clone()],
                ret: Box::new(socket.clone()),
                err: Some(Box::new(s.clone())),
            }),
        );
        self.env.define(
            "tcp_read",
            mono(InferType::IFun {
                params: vec![socket.clone()],
                ret: Box::new(s.clone()),
                err: Some(Box::new(s.clone())),
            }),
        );
        self.env.define(
            "tcp_write",
            mono(InferType::IFun {
                params: vec![socket.clone(), s.clone()],
                ret: Box::new(n.clone()),
                err: Some(Box::new(s.clone())),
            }),
        );
        self.env.define(
            "tcp_close",
            mono(InferType::IFun {
                params: vec![socket],
                ret: Box::new(n),
                err: Some(Box::new(s)),
            }),
        );
    }
}
