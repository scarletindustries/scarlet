-- closures: map, filter and fold over a 1000-element array with closures that
-- capture variables, a composed pipeline of small functions, and a counter
-- whose step function returns the next counter. It stresses making closures
-- and calling them. Written for this suite, not a port.

-- Lua has no standard map, filter or fold, so these are the usual ones.
local function map(xs, f)
  local out = {}
  for i = 1, #xs do out[i] = f(xs[i]) end
  return out
end

local function filter(xs, p)
  local out, n = {}, 0
  for i = 1, #xs do
    local x = xs[i]
    if p(x) then n = n + 1; out[n] = x end
  end
  return out
end

local function fold(xs, init, f)
  local acc = init
  for i = 1, #xs do acc = f(acc, xs[i]) end
  return acc
end

local function adder(k) return function(x) return x + k end end

local function scaler(k, m) return function(x) return x * k % m end end

local function compose(f, g) return function(x) return g(f(x)) end end

local function counter(value, step)
  return {
    value = value,
    next = function(d) return counter((value + step * d) % 1000003, step + 1) end,
  }
end

local function count(c, n)
  while n ~= 0 do
    c = c.next(n % 5 + 1)
    n = n - 1
  end
  return c.value
end

local function rounds(xs, r, acc)
  while r ~= 0 do
    local k = r % 7
    local scaled = map(xs, function(x) return (x * r + acc) % 1009 end)
    local kept = filter(scaled, function(x) return x % 7 ~= k end)
    local direct = fold(kept, 0, function(s, x) return (s + x * r) % 1000003 end)
    local steps = { adder(r), scaler(3, 1013), adder(acc % 13), scaler(k + 2, 1019) }
    local pipeline = fold(steps, function(x) return x end, compose)
    local piped = fold(map(kept, pipeline), 0, function(s, x) return (s + x) % 1000003 end)
    local counted = count(counter(acc % 97, r), 200)
    acc = (acc * 31 + direct + piped + counted) % 1000003
    r = r - 1
  end
  return acc
end

local xs = {}
for i = 0, 999 do xs[i + 1] = i end
print(rounds(xs, 1500, 1))
