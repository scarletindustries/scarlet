-- Permute: walks the permutations of a six-element array by swapping in place.
-- It stresses array reads and writes and deep recursion.
-- Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet), MIT licence.

local v, count, check

local function swap(i, j)
  local tmp = v[i]
  v[i] = v[j]
  v[j] = tmp
end

local function permute(n)
  count = count + 1
  if n == 0 then
    check = (check * 31 + v[1]) % 1000003
  else
    local n1 = n - 1
    permute(n1)
    for i = n, 1, -1 do
      swap(n, i)
      permute(n1)
      swap(n, i)
    end
  end
end

local total = 0
check = 0
for _ = 1, 200 do
  count = 0
  v = {0, 1, 2, 3, 4, 5}
  permute(6)
  total = total + count
end
print(total)
print(check)
