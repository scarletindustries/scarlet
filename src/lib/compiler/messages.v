module compiler

pub enum MessageType {
	error
	warning
	notice
}

@[heap]
pub struct Message {
	// Must be called typ because type is a reserved word in V
	typ MessageType = .notice
}
