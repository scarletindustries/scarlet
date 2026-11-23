-- NBody: five bodies (the sun and four planets) pull on each other for many
-- small time steps. It stresses float arithmetic and record fields. Ported from
-- the Are We Fast Yet suite (MIT licence), which took it from the Computer
-- Language Benchmarks Game. The square root is Newton's method written out, the
-- same in every language, because Scarlet has none.

local PI            = 3.141592653589793
local SOLAR_MASS    = 4.0 * PI * PI
local DAYS_PER_YEAR = 365.24
local STEPS         = 100000

local function body (x, y, z, vx, vy, vz, mass)
    return {
        x = x,
        y = y,
        z = z,
        vx = vx * DAYS_PER_YEAR,
        vy = vy * DAYS_PER_YEAR,
        vz = vz * DAYS_PER_YEAR,
        mass = mass * SOLAR_MASS,
    }
end

local function sqrt (d)
    local guess = 1.0
    if d > 1.0 then guess = d end
    while true do
        local next = 0.5 * (guess + d / guess)
        if next >= guess then return guess end
        guess = next
    end
end

local function create_bodies ()
    local bodies = {
        body(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0),
        body( 4.84143144246472090,
             -1.16032004402742839,
             -0.103622044471123109,
              0.00166007664274403694,
              0.00769901118419740425,
             -0.0000690460016972063023,
              0.000954791938424326609),
        body( 8.34336671824457987,
              4.12479856412430479,
             -0.403523417114321381,
             -0.00276742510726862411,
              0.00499852801234917238,
              0.0000230417297573763929,
              0.000285885980666130812),
        body( 12.8943695621391310,
             -15.1111514016986312,
              -0.223307578892655734,
               0.00296460137564761618,
               0.00237847173959480950,
              -0.0000296589568540237556,
               0.0000436624404335156298),
        body( 15.3796971148509165,
             -25.9193146099879641,
               0.179258772950371181,
               0.00268067772490389322,
               0.00162824170038242295,
              -0.0000951592254519715870,
               0.0000515138902046611451),
    }
    local px, py, pz = 0.0, 0.0, 0.0
    for i = 1, #bodies do
        local b = bodies[i]
        px = px + b.vx * b.mass
        py = py + b.vy * b.mass
        pz = pz + b.vz * b.mass
    end
    local sun = bodies[1]
    sun.vx = 0.0 - px / SOLAR_MASS
    sun.vy = 0.0 - py / SOLAR_MASS
    sun.vz = 0.0 - pz / SOLAR_MASS
    return bodies
end

local function advance (bodies, dt)
    for i = 1, #bodies do
        local i_body = bodies[i]

        for j = i + 1, #bodies do
            local j_body = bodies[j]
            local dx = i_body.x - j_body.x
            local dy = i_body.y - j_body.y
            local dz = i_body.z - j_body.z

            local d_squared = dx * dx + dy * dy + dz * dz
            local distance = sqrt(d_squared)
            local mag = dt / (d_squared * distance)

            i_body.vx = i_body.vx - dx * j_body.mass * mag
            i_body.vy = i_body.vy - dy * j_body.mass * mag
            i_body.vz = i_body.vz - dz * j_body.mass * mag

            j_body.vx = j_body.vx + dx * i_body.mass * mag
            j_body.vy = j_body.vy + dy * i_body.mass * mag
            j_body.vz = j_body.vz + dz * i_body.mass * mag
        end
    end

    for i = 1, #bodies do
        local b = bodies[i]
        b.x = b.x + dt * b.vx
        b.y = b.y + dt * b.vy
        b.z = b.z + dt * b.vz
    end
end

local function energy (bodies)
    local e = 0.0

    for i = 1, #bodies do
        local i_body = bodies[i]

        e = e + 0.5 * i_body.mass * (i_body.vx * i_body.vx +
                                     i_body.vy * i_body.vy +
                                     i_body.vz * i_body.vz)

        for j = i + 1, #bodies do
            local j_body = bodies[j]
            local dx = i_body.x - j_body.x
            local dy = i_body.y - j_body.y
            local dz = i_body.z - j_body.z

            local distance = sqrt(dx * dx + dy * dy + dz * dz)
            e = e - i_body.mass * j_body.mass / distance
        end
    end
    return e
end

-- The energy as a whole number, so every language prints the same text.
local function scaled (e)
    local n = e * 1000000000000000.0
    if n < 0 then n = math.ceil(n) else n = math.floor(n) end
    return string.format('%.0f', n)
end

local bodies = create_bodies()
print(scaled(energy(bodies)))
for _ = 1, STEPS do
    advance(bodies, 0.01)
end
print(scaled(energy(bodies)))
