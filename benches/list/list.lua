-- AWFY List: builds linked lists of a user-defined type and runs the recursive
-- tail (Takeuchi) comparison over them. It stresses allocation and recursion.
-- Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence).

local Element = {}
Element.__index = Element

function Element.new(v)
    return setmetatable({val = v, next = nil}, Element)
end

function Element:length()
    if not self.next then
        return 1
    else
        return 1 + self.next:length()
    end
end

local function make_list(length)
    if length == 0 then
        return nil
    else
        local e = Element.new(length)
        e.next = make_list(length - 1)
        return e
    end
end

local function is_shorter_than(x, y)
    local x_tail, y_tail = x, y
    while y_tail do
        if not x_tail then
            return true
        end
        x_tail = x_tail.next
        y_tail = y_tail.next
    end
    return false
end

local function tail(x, y, z)
    if is_shorter_than(y, x) then
        return tail(tail(x.next, y, z),
                    tail(y.next, z, x),
                    tail(z.next, x, y))
    else
        return z
    end
end

local function benchmark()
    local result = tail(make_list(15), make_list(10), make_list(6))
    return result:length()
end

local total = 0
for _ = 1, 2000 do
    total = total + benchmark()
end
print(total)
