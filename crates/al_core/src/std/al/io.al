import al/binary

// Typed errors for filesystem operations, mirroring Rust's std::io::ErrorKind
// (the POSIX/errno failure set). Path-bearing variants carry the path they
// concern so a caller can report or recover without re-deriving it.
pub type IoError {
	// No file or directory at this path. (ENOENT)
	NotFound(path String)
	// The process lacks permission for this path. (EACCES)
	PermissionDenied(path String)
	// A file already exists where one was required to be new. (EEXIST)
	AlreadyExists(path String)
	// A path component used as a directory is not one. (ENOTDIR)
	NotADirectory(path String)
	// The path is a directory where a file was expected. (EISDIR)
	IsADirectory(path String)
	// The filesystem is mounted read-only. (EROFS)
	ReadOnlyFilesystem(path String)
	// Too many symbolic links while resolving the path. (ELOOP)
	FilesystemLoop(path String)
	// The file's contents are not valid for the operation (e.g. not UTF-8).
	InvalidData(path String)
	// No space left on the device. (ENOSPC)
	StorageFull
	// The user's disk quota is exhausted. (EDQUOT)
	QuotaExceeded
	// The file is larger than the filesystem or process allows. (EFBIG)
	FileTooLarge(path String)
	// A write was handed a binary that is not a whole number of bytes.
	UnalignedBinary
	// An OS error with no dedicated variant above, carrying its raw errno.
	Errno(code Int)
}

@vm(io__read_file)
pub fn read_file(path String) Result(Binary, IoError)

@vm(io__write_file)
pub fn write_file(path String, data Binary) Result(Nil, IoError)

pub fn read_text(path String) Result(String, IoError) {
	match read_file(path) {
		Ok(b) -> match binary.to_string(b) {
			Ok(s) -> Ok(s)
			Err(Nil) -> Err(InvalidData(path))
		}
		Err(e) -> Err(e)
	}
}

pub fn write_text(path String, s String) Result(Nil, IoError) {
	write_file(path, binary.from_string(s))
}
