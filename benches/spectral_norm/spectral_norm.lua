-- spectral-norm: the power method on an implicit matrix, 10 rounds. It stresses
-- float arithmetic over arrays. Ported from the Computer Language Benchmarks
-- Game (benchmarksgame-team.pages.debian.net/benchmarksgame, BSD-3 licence).

-- Entry (i, j) of the infinite matrix, counting from zero.
local function A(i, j)
  return 1.0 / ((i + j) * (i + j + 1) / 2 + i + 1)
end

local function Av(x, y, N)
  for i = 0, N - 1 do
    local a = 0.0
    for j = 0, N - 1 do a = a + x[j + 1] * A(i, j) end
    y[i + 1] = a
  end
end

local function Atv(x, y, N)
  for i = 0, N - 1 do
    local a = 0.0
    for j = 0, N - 1 do a = a + x[j + 1] * A(j, i) end
    y[i + 1] = a
  end
end

local function AtAv(x, y, t, N)
  Av(x, t, N)
  Atv(t, y, N)
end

-- The same 30 Newton steps in every language, as Scarlet has no square root.
local function sqrt(s)
  local x = 1.0
  for _ = 1, 30 do x = 0.5 * (x + s / x) end
  return x
end

-- Prints the norm, scaled to an integer, and then floor(u.v). The second number
-- depends on every round, where the norm alone has converged after three.
local function spectral_norm(N)
  local u, v, t = {}, {}, {}
  for i = 1, N do u[i] = 1.0 end
  for _ = 1, 10 do
    AtAv(u, v, t, N)
    AtAv(v, u, t, N)
  end
  local vBv, vv = 0.0, 0.0
  for i = 1, N do
    local ui, vi = u[i], v[i]
    vBv = vBv + ui * vi
    vv = vv + vi * vi
  end
  print(string.format("%d", math.floor(sqrt(vBv / vv) * 1000000000.0 + 0.5)))
  print(string.format("%d", math.floor(vBv)))
end

spectral_norm(100)
spectral_norm(700)
