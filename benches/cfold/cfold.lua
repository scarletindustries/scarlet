-- cfold: build an expression tree, reassociate it and fold its constants, many times over.
-- It stresses allocating small nodes of a sum type and matching on them, with deep recursion.
-- Ported from test/bench/koka/cfold.kk in Koka's benchmarks (Apache-2.0), itself from Lean 4's const_fold.

-- A node is {tag, a, b}: a Var or Val keeps its number in slot 2.
local VAR, VAL, ADD, MUL = 1, 2, 3, 4

local function mk_expr(n, v)
  if n == 0 then
    if v == 0 then
      return { VAR, 1 }
    else
      return { VAL, v }
    end
  else
    return { ADD, mk_expr(n - 1, v + 1), mk_expr(n - 1, v == 0 and 0 or v - 1) }
  end
end

local function append_add(e0, e3)
  if e0[1] == ADD then
    return { ADD, e0[2], append_add(e0[3], e3) }
  else
    return { ADD, e0, e3 }
  end
end

local function append_mul(e0, e3)
  if e0[1] == MUL then
    return { MUL, e0[2], append_mul(e0[3], e3) }
  else
    return { MUL, e0, e3 }
  end
end

local function reassoc(e)
  local tag = e[1]
  if tag == ADD then
    return append_add(reassoc(e[2]), reassoc(e[3]))
  elseif tag == MUL then
    return append_mul(reassoc(e[2]), reassoc(e[3]))
  else
    return e
  end
end

local function cfold(e)
  local tag = e[1]
  if tag == ADD then
    local f1 = cfold(e[2])
    local f2 = cfold(e[3])
    if f1[1] == VAL then
      local a = f1[2]
      local tag2 = f2[1]
      if tag2 == VAL then
        return { VAL, a + f2[2] }
      elseif tag2 == ADD then
        local l, r = f2[2], f2[3]
        if r[1] == VAL then
          return { ADD, { VAL, a + r[2] }, l }
        elseif l[1] == VAL then
          return { ADD, { VAL, a + l[2] }, r }
        end
      end
    end
    return { ADD, f1, f2 }
  elseif tag == MUL then
    local f1 = cfold(e[2])
    local f2 = cfold(e[3])
    if f1[1] == VAL then
      local a = f1[2]
      local tag2 = f2[1]
      if tag2 == VAL then
        return { VAL, a * f2[2] }
      elseif tag2 == MUL then
        local l, r = f2[2], f2[3]
        if r[1] == VAL then
          return { MUL, { VAL, a * r[2] }, l }
        elseif l[1] == VAL then
          return { MUL, { VAL, a * l[2] }, r }
        end
      end
    end
    return { MUL, f1, f2 }
  else
    return e
  end
end

local function eval(e)
  local tag = e[1]
  if tag == VAR then
    return 0
  elseif tag == VAL then
    return e[2]
  elseif tag == ADD then
    return eval(e[2]) + eval(e[3])
  else
    return eval(e[2]) * eval(e[3])
  end
end

-- An order-sensitive hash of the tree, so the output pins down the folded tree's
-- shape and not only its value, which folding leaves unchanged.
local function shape(e)
  local tag = e[1]
  if tag == VAR then
    return 3 * e[2] + 1
  elseif tag == VAL then
    return 3 * e[2] + 2
  elseif tag == ADD then
    return (31 * shape(e[2]) + 17 * shape(e[3]) + 3) % 1000003
  else
    return (37 * shape(e[2]) + 19 * shape(e[3]) + 5) % 1000003
  end
end

-- The tree is rebuilt every round, from a start value that walks 0 to 7, so some
-- trees are mostly Var leaves and some mostly Val. Depth 12 is as deep as it goes
-- because the reassociated tree is one chain 4096 long, and LuaJIT's stack ends near 10000.
local before, after, shapes = 0, 0, 0
for i = 0, 699 do
  local e = mk_expr(12, i % 8)
  before = before + eval(e)
  local f = cfold(reassoc(e))
  after = after + eval(f)
  shapes = shapes + shape(f)
end
print(before)
print(after)
print(shapes)
