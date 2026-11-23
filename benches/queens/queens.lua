-- Queens from the Are We Fast Yet suite: place eight queens by backtracking over
-- boolean arrays of free rows and diagonals. Stresses array reads and writes.
-- Ported from https://github.com/smarr/are-we-fast-yet (MIT licence).

local queens = {}

function queens:queens ()
    self.free_rows  = {true, true, true, true, true, true, true, true}
    self.free_maxs  = {true, true, true, true, true, true, true, true,
                       true, true, true, true, true, true, true, true}
    self.free_mins  = {true, true, true, true, true, true, true, true,
                       true, true, true, true, true, true, true, true}
    self.queen_rows = {-1,   -1,   -1,   -1,   -1,   -1,   -1,   -1}
    return self:place_queen(1)
end

function queens:place_queen (c)
    for r = 1, 8 do
        if self:get_row_column(r, c) then
            self.queen_rows[r] = c
            self:set_row_column(r, c, false)
            if c == 8 then
                return true
            end
            if self:place_queen(c + 1) then
                return true
            end
            self:set_row_column(r, c, true)
        end
    end
    return false
end

function queens:get_row_column (r, c)
    return self.free_rows[r] and self.free_maxs[c + r] and self.free_mins[c - r + 8]
end

function queens:set_row_column (r, c, v)
    self.free_rows[r        ] = v
    self.free_maxs[c + r    ] = v
    self.free_mins[c - r + 8] = v
end

-- The column of each row's queen, counted from zero, read as one decimal number.
function queens:digits ()
    local acc = 0
    for r = 1, 8 do
        acc = acc * 10 + self.queen_rows[r] - 1
    end
    return acc
end

local solved, sum = 0, 0
for _ = 1, 3500 do
    if queens:queens() then
        solved = solved + 1
        sum = sum + queens:digits()
    end
end
print(string.format('%d', solved))
print(string.format('%d', sum))
