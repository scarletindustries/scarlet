-- deriv: differentiate x^x symbolically nine times over, simplifying as it goes, and do that twice.
-- It stresses allocating small nodes of a sum type and matching on pairs of them.
-- Ported from test/bench/koka/deriv.kk in Koka's benchmarks (Apache-2.0), itself from Lean 4's deriv.ml.

local VAL, VAR, ADD, MUL, POW, LN = 0, 1, 2, 3, 4, 5

local function pown(a, b)
  local acc = 1
  while b > 0 do
    acc = acc * a
    b = b - 1
  end
  return acc
end

local add, mul

function add(n0, m0)
  local mt, nt = m0[1], n0[1]
  if mt == VAL then
    local m = m0[2]
    if nt == VAL then return { VAL, n0[2] + m } end
    if m == 0 then return n0 end
    return add({ VAL, m }, n0)
  end
  if mt == ADD then
    local ml = m0[2]
    if nt == VAL then
      local n = n0[2]
      if n == 0 then return m0 end
      if ml[1] == VAL then return add({ VAL, n + ml[2] }, m0[3]) end
      return { ADD, n0, m0 }
    end
    if ml[1] == VAL then return add({ VAL, ml[2] }, add(n0, m0[3])) end
    if nt == ADD then return add(n0[2], add(n0[3], m0)) end
    return { ADD, n0, m0 }
  end
  if nt == VAL then
    if n0[2] == 0 then return m0 end
    return { ADD, n0, m0 }
  end
  if nt == ADD then return add(n0[2], add(n0[3], m0)) end
  return { ADD, n0, m0 }
end

function mul(n0, m0)
  local mt, nt = m0[1], n0[1]
  if mt == VAL then
    local m = m0[2]
    if nt == VAL then return { VAL, n0[2] * m } end
    if m == 0 then return { VAL, 0 } end
    if m == 1 then return n0 end
    return mul({ VAL, m }, n0)
  end
  if mt == MUL then
    local ml = m0[2]
    if nt == VAL then
      local n = n0[2]
      if n == 0 then return { VAL, 0 } end
      if n == 1 then return m0 end
      if ml[1] == VAL then return mul({ VAL, n * ml[2] }, m0[3]) end
      return { MUL, n0, m0 }
    end
    if ml[1] == VAL then return mul({ VAL, ml[2] }, mul(n0, m0[3])) end
    if nt == MUL then return mul(n0[2], mul(n0[3], m0)) end
    return { MUL, n0, m0 }
  end
  if nt == VAL then
    local n = n0[2]
    if n == 0 then return { VAL, 0 } end
    if n == 1 then return m0 end
    return { MUL, n0, m0 }
  end
  if nt == MUL then return mul(n0[2], mul(n0[3], m0)) end
  return { MUL, n0, m0 }
end

local function powr(m0, n0)
  if n0[1] == VAL then
    local n = n0[2]
    if m0[1] == VAL then return { VAL, pown(m0[2], n) } end
    if n == 0 then return { VAL, 1 } end
    if n == 1 then return m0 end
    return { POW, m0, n0 }
  end
  if m0[1] == VAL and m0[2] == 0 then return { VAL, 0 } end
  return { POW, m0, n0 }
end

local function ln(n)
  if n[1] == VAL and n[2] == 1 then return { VAL, 0 } end
  return { LN, n }
end

local function d(x, e)
  local t = e[1]
  if t == VAL then return { VAL, 0 } end
  if t == VAR then
    if x == e[2] then return { VAL, 1 } else return { VAL, 0 } end
  end
  if t == ADD then return add(d(x, e[2]), d(x, e[3])) end
  if t == MUL then
    local f, g = e[2], e[3]
    return add(mul(f, d(x, g)), mul(g, d(x, f)))
  end
  if t == POW then
    local f, g = e[2], e[3]
    return mul(
      powr(f, g),
      add(mul(mul(g, d(x, f)), powr(f, { VAL, -1 })), mul(ln(f), d(x, g)))
    )
  end
  local f = e[2]
  return mul(d(x, f), powr(f, { VAL, -1 }))
end

local function count(e)
  local t = e[1]
  if t == VAL or t == VAR then return 1 end
  if t == LN then return count(e[2]) end
  return count(e[2]) + count(e[3])
end

local function nest(n, f)
  for i = 1, n do
    f = d("x", f)
    print(i .. " count: " .. count(f))
  end
  return f
end

local x = { VAR, "x" }
nest(9, powr(x, x))
nest(9, powr(x, x))
print("done")
