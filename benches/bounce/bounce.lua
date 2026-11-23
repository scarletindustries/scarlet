-- Bounce: 100 balls bounce around a box for 50 steps. We count the bounces and
-- add up where the balls end.
-- It stresses record allocation, field reads and small-integer arithmetic.
-- Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence).

local band = require('bit').band
local abs = math.abs

-- The suite's own generator.
local Random = {}
Random.__index = Random

function Random.new()
  return setmetatable({seed = 74755}, Random)
end

function Random:next()
  self.seed = band(self.seed * 1309 + 13849, 65535)
  return self.seed
end

local Ball = {}
Ball.__index = Ball

function Ball.new(random)
  local obj = {
    x = random:next() % 500,
    y = random:next() % 500,
    x_vel = (random:next() % 300) - 150,
    y_vel = (random:next() % 300) - 150,
  }
  return setmetatable(obj, Ball)
end

function Ball:bounce()
  local x_limit, y_limit = 500, 500
  local bounced = false
  self.x = self.x + self.x_vel
  self.y = self.y + self.y_vel
  if self.x > x_limit then
    self.x = x_limit
    self.x_vel = 0 - abs(self.x_vel)
    bounced = true
  end
  if self.x < 0 then
    self.x = 0
    self.x_vel = abs(self.x_vel)
    bounced = true
  end
  if self.y > y_limit then
    self.y = y_limit
    self.y_vel = 0 - abs(self.y_vel)
    bounced = true
  end
  if self.y < 0 then
    self.y = 0
    self.y_vel = abs(self.y_vel)
    bounced = true
  end
  return bounced
end

local function benchmark()
  local random = Random.new()
  local ball_count = 100
  local bounces = 0
  local balls = {}

  for i = 1, ball_count do
    balls[i] = Ball.new(random)
  end

  for _ = 1, 50 do
    for i = 1, #balls do
      local ball = balls[i]
      if ball:bounce() then
        bounces = bounces + 1
      end
    end
  end

  local positions = 0
  for i = 1, #balls do
    local ball = balls[i]
    positions = positions + ball.x + ball.y
  end
  return bounces, positions
end

-- One run gives the bounce count and the sum of every ball's final x and y.
print(string.format('%d %d', benchmark()))
local bounces, positions = 0, 0
for _ = 1, 1000 do
  local b, p = benchmark()
  bounces = bounces + b
  positions = positions + p
end
print(string.format('%d %d', bounces, positions))
