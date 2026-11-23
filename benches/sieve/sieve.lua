-- sieve: the sieve of Eratosthenes over an array of 5000 flags, run many times.
-- It stresses array writes in a tight loop. Ported from the Are We Fast Yet
-- suite (https://github.com/smarr/are-we-fast-yet, MIT licence).

local function sieve(flags, size)
    local prime_count = 0
    for i = 2, size do
        if flags[i - 1] then
            prime_count = prime_count + 1
            local k = i + i
            while k <= size do
                flags[k - 1] = false
                k = k + i
            end
        end
    end
    return prime_count
end

local total = 0
for _ = 1, 45 do
    local flags = {}
    for i = 1, 5000 do
        flags[i] = true
    end
    total = total + sieve(flags, 5000)
end
print(total)
