module downloader

import os
import net.http

pub struct ProgressDownloader {
mut:
	path string
	f    os.File
}

pub fn (mut d ProgressDownloader) on_start(mut request http.Request, path string) ! {
	d.path = path
	d.f = os.create(path)!
	print('[${' '.repeat(30)}] 0% connecting...')
	os.flush()
}

pub fn (mut d ProgressDownloader) on_chunk(request &http.Request, chunk []u8, received u64, expected u64) ! {
	d.f.write(chunk)!

	bar_width := 30
	pct := if expected > 0 { received * 100 / expected } else { u64(0) }
	filled := if expected > 0 { int(received * u64(bar_width) / expected) } else { 0 }
	bar := '█'.repeat(filled) + '░'.repeat(bar_width - filled)
	print('\r\x1b[K[${bar}] ${pct}% ${format_bytes(received)}/${format_bytes(expected)}')
	os.flush()
}

pub fn (mut d ProgressDownloader) on_finish(request &http.Request, response &http.Response) ! {
	d.f.close()
	println('')
}

fn format_bytes(bytes u64) string {
	if bytes < 1024 {
		return '${bytes}B'
	} else if bytes < 1024 * 1024 {
		return '${f64(bytes) / 1024:.1}KB'
	} else {
		return '${f64(bytes) / (1024 * 1024):.1}MB'
	}
}
