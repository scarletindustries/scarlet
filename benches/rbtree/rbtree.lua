-- Stresses pattern matching on nested constructors, allocation, and reuse of
-- nodes in place: insert into a functional red-black tree (Okasaki balance), then fold.
-- Ported from Koka's test/bench/koka/rbtree.kk (https://github.com/koka-lang/koka, Apache-2.0).

local RED, BLACK = 0, 1

-- A node is a table; a leaf is nil.
local function node(c, l, k, v, r)
  return { c = c, l = l, k = k, v = v, r = r }
end

local function is_red(t)
  return t ~= nil and t.c == RED
end

local function balance_left(l, k, v, r)
  if l == nil then return nil end
  local ll, lr = l.l, l.r
  if is_red(ll) then
    return node(RED, node(BLACK, ll.l, ll.k, ll.v, ll.r), l.k, l.v, node(BLACK, lr, k, v, r))
  elseif is_red(lr) then
    return node(RED, node(BLACK, ll, l.k, l.v, lr.l), lr.k, lr.v, node(BLACK, lr.r, k, v, r))
  else
    return node(BLACK, node(RED, ll, l.k, l.v, lr), k, v, r)
  end
end

local function balance_right(l, k, v, r)
  if r == nil then return nil end
  local rl, rr = r.l, r.r
  if is_red(rl) then
    return node(RED, node(BLACK, l, k, v, rl.l), rl.k, rl.v, node(BLACK, rl.r, r.k, r.v, rr))
  elseif is_red(rr) then
    return node(RED, node(BLACK, l, k, v, rl), r.k, r.v, node(BLACK, rr.l, rr.k, rr.v, rr.r))
  else
    return node(BLACK, l, k, v, node(RED, rl, r.k, r.v, rr))
  end
end

local function ins(t, k, v)
  if t == nil then
    return node(RED, nil, k, v, nil)
  end
  local kx = t.k
  if t.c == RED then
    if k < kx then
      return node(RED, ins(t.l, k, v), kx, t.v, t.r)
    elseif k > kx then
      return node(RED, t.l, kx, t.v, ins(t.r, k, v))
    else
      return node(RED, t.l, k, v, t.r)
    end
  else
    if k < kx then
      if is_red(t.l) then
        return balance_left(ins(t.l, k, v), kx, t.v, t.r)
      else
        return node(BLACK, ins(t.l, k, v), kx, t.v, t.r)
      end
    elseif k > kx then
      if is_red(t.r) then
        return balance_right(t.l, kx, t.v, ins(t.r, k, v))
      else
        return node(BLACK, t.l, kx, t.v, ins(t.r, k, v))
      end
    else
      return node(BLACK, t.l, k, v, t.r)
    end
  end
end

local function set_black(t)
  if t == nil then return t end
  return node(BLACK, t.l, t.k, t.v, t.r)
end

local function insert(t, k, v)
  return set_black(ins(t, k, v))
end

local function fold(t, b, f)
  if t == nil then return b end
  return fold(t.r, f(t.k, t.v, fold(t.l, b, f)), f)
end

local function height(t)
  if t == nil then return 0 end
  local hl = height(t.l)
  local hr = height(t.r)
  if hl > hr then return hl + 1 else return hr + 1 end
end

local function make_tree(n)
  local t = nil
  while n > 0 do
    n = n - 1
    t = insert(t, n, n % 10 == 0)
  end
  return t
end

local t = make_tree(300000)
local count = fold(t, 0, function(k, v, r)
  if v then return r + 1 else return r end
end)
print(count)
print(fold(t, 0, function(k, v, r)
  return (r * 31 + k) % 1000000007
end))
print(height(t))
