-- Strings: builds records by interpolation and concatenation, trims, splits,
-- joins and replaces them, searches for substrings and turns ints into strings
-- and back. It stresses the string built-ins. Written for this suite.

local ROWS = 120000
local BATCH_SIZE = 50
local WORDS = {
  "amber", "birch", "cedar", "delta", "ember", "flint", "grove", "heath",
  "ivory", "jade", "kelp", "lotus", "maple", "north", "olive", "pearl",
}

local find, sub, gsub, concat = string.find, string.sub, string.gsub, table.concat

local function next_seed(seed)
  return (seed * 1309 + 13849) % 65536
end

local function mix(h, v)
  return (h * 31 + v) % 1000000007
end

local function count(flag)
  if flag then return 1 else return 0 end
end

-- Lua has no split, so this is the usual one over a plain-text find.
local function split(s, sep)
  local parts, n, from = {}, 0, 1
  local width = #sep
  while true do
    local at = find(s, sep, from, true)
    n = n + 1
    if not at then
      parts[n] = sub(s, from)
      return parts, n
    end
    parts[n] = sub(s, from, at - 1)
    from = at + width
  end
end

local function trim(s)
  return (gsub(s, "^%s*(.-)%s*$", "%1"))
end

local function contains(s, needle)
  return find(s, needle, 1, true) ~= nil
end

local function field_value(field)
  local parts, n = split(field, "=")
  if n ~= 2 then return 0 end
  local key, value = parts[1], parts[2]
  if key == "id" or key == "score" then return tonumber(value) end
  return #value
end

local function field_text(field)
  local parts, n = split(field, "=")
  if n ~= 2 then return "" end
  return parts[2]
end

local function row_checksum(h, i, a, b, c, row)
  local padded = "  " .. row .. " "
  local fields, n = split(trim(padded), ",")
  local texts = {}
  for k = 1, n do
    h = mix(h, field_value(fields[k]))
  end
  for k = 1, n do
    texts[k] = field_text(fields[k])
  end
  local rebuilt = concat(texts, "|")
  h = mix(h, #rebuilt)
  h = mix(h, count(contains(row, "7,tag=")) + 2 * count(contains(row, "ar-ma")))
  h = mix(h, #(gsub(row, "=", ": ")))
  local digits = tostring(a * 65536 + b) .. tostring(c)
  local number = tonumber(digits)
  return mix(mix(h, number % 1000003), i)
end

local function batch_checksum(h, batch)
  local text = concat(batch, "\n")
  local lines, line_count = split(text, "\n")
  local _, piece_count = split(text, ",")
  local hits = 0
  for k = 1, line_count do
    hits = hits + count(contains(lines[k], "user=u1"))
  end
  h = mix(h, #text)
  h = mix(h, line_count * 1000 + hits)
  h = mix(h, piece_count)
  return mix(h, #(gsub(text, "tag=", "#")))
end

local function main()
  local seed, h = 42, 7
  local batch, size = {}, 0
  for i = 0, ROWS - 1 do
    local a = next_seed(seed)
    local b = next_seed(a)
    local c = next_seed(b)
    seed = c
    local row = "id=" .. i .. ",user=u" .. (a % 997) .. ",score=" .. b
      .. ",tag=" .. WORDS[c % 16 + 1] .. "-" .. WORDS[a % 16 + 1]
    h = row_checksum(h, i, a, b, c, row)
    size = size + 1
    batch[size] = row
    if size == BATCH_SIZE then
      h = batch_checksum(h, batch)
      batch, size = {}, 0
    end
  end
  print(string.format("%d", h))
end

main()
