import al/binary

// Reason phrases for HTTP status codes, per the IANA registry / RFC 9110.
// Each phrase is hoisted into a top-level `const` so the backing binary is
// built once at module load: a `binary.from_string` call inside the match
// would allocate a fresh binary on every lookup (binary literals are not
// constant-folded).

// 1xx informational
const rp_continue = <<'Continue'>>
const rp_switching_protocols = <<'Switching Protocols'>>

// 2xx success
const rp_ok = <<'OK'>>
const rp_created = <<'Created'>>
const rp_accepted = <<'Accepted'>>
const rp_non_authoritative = <<'Non-Authoritative Information'>>
const rp_no_content = <<'No Content'>>
const rp_reset_content = <<'Reset Content'>>
const rp_partial_content = <<'Partial Content'>>

// 3xx redirection
const rp_multiple_choices = <<'Multiple Choices'>>
const rp_moved_permanently = <<'Moved Permanently'>>
const rp_found = <<'Found'>>
const rp_see_other = <<'See Other'>>
const rp_not_modified = <<'Not Modified'>>
const rp_temporary_redirect = <<'Temporary Redirect'>>
const rp_permanent_redirect = <<'Permanent Redirect'>>

// 4xx client error
const rp_bad_request = <<'Bad Request'>>
const rp_unauthorized = <<'Unauthorized'>>
const rp_payment_required = <<'Payment Required'>>
const rp_forbidden = <<'Forbidden'>>
const rp_not_found = <<'Not Found'>>
const rp_method_not_allowed = <<'Method Not Allowed'>>
const rp_not_acceptable = <<'Not Acceptable'>>
const rp_proxy_auth_required = <<'Proxy Authentication Required'>>
const rp_request_timeout = <<'Request Timeout'>>
const rp_conflict = <<'Conflict'>>
const rp_gone = <<'Gone'>>
const rp_length_required = <<'Length Required'>>
const rp_precondition_failed = <<'Precondition Failed'>>
const rp_content_too_large = <<'Content Too Large'>>
const rp_uri_too_long = <<'URI Too Long'>>
const rp_unsupported_media_type = <<'Unsupported Media Type'>>
const rp_range_not_satisfiable = <<'Range Not Satisfiable'>>
const rp_expectation_failed = <<'Expectation Failed'>>
const rp_misdirected_request = <<'Misdirected Request'>>
const rp_unprocessable_content = <<'Unprocessable Content'>>
const rp_upgrade_required = <<'Upgrade Required'>>
const rp_precondition_required = <<'Precondition Required'>>
const rp_too_many_requests = <<'Too Many Requests'>>
const rp_header_fields_too_large = <<'Request Header Fields Too Large'>>
const rp_unavailable_legal = <<'Unavailable For Legal Reasons'>>

// 5xx server error
const rp_internal_server_error = <<'Internal Server Error'>>
const rp_not_implemented = <<'Not Implemented'>>
const rp_bad_gateway = <<'Bad Gateway'>>
const rp_service_unavailable = <<'Service Unavailable'>>
const rp_gateway_timeout = <<'Gateway Timeout'>>
const rp_http_version_not_supported = <<'HTTP Version Not Supported'>>

// Unknown / unregistered codes get an empty reason phrase, which is a valid
// (zero-length) reason-phrase in the status line per RFC 9110 section 15.
const rp_unknown = <<>>

// The reason phrase for a status code, e.g. 404 -> "Not Found". Returns an
// empty binary for codes not in the registry.
pub fn reason_phrase(code Int) Binary {
	match code {
		100 -> rp_continue
		101 -> rp_switching_protocols
		200 -> rp_ok
		201 -> rp_created
		202 -> rp_accepted
		203 -> rp_non_authoritative
		204 -> rp_no_content
		205 -> rp_reset_content
		206 -> rp_partial_content
		300 -> rp_multiple_choices
		301 -> rp_moved_permanently
		302 -> rp_found
		303 -> rp_see_other
		304 -> rp_not_modified
		307 -> rp_temporary_redirect
		308 -> rp_permanent_redirect
		400 -> rp_bad_request
		401 -> rp_unauthorized
		402 -> rp_payment_required
		403 -> rp_forbidden
		404 -> rp_not_found
		405 -> rp_method_not_allowed
		406 -> rp_not_acceptable
		407 -> rp_proxy_auth_required
		408 -> rp_request_timeout
		409 -> rp_conflict
		410 -> rp_gone
		411 -> rp_length_required
		412 -> rp_precondition_failed
		413 -> rp_content_too_large
		414 -> rp_uri_too_long
		415 -> rp_unsupported_media_type
		416 -> rp_range_not_satisfiable
		417 -> rp_expectation_failed
		421 -> rp_misdirected_request
		422 -> rp_unprocessable_content
		426 -> rp_upgrade_required
		428 -> rp_precondition_required
		429 -> rp_too_many_requests
		431 -> rp_header_fields_too_large
		451 -> rp_unavailable_legal
		500 -> rp_internal_server_error
		501 -> rp_not_implemented
		502 -> rp_bad_gateway
		503 -> rp_service_unavailable
		504 -> rp_gateway_timeout
		505 -> rp_http_version_not_supported
		else -> rp_unknown
	}
}
