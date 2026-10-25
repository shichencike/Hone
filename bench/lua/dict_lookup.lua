-- 基准 7：字典查找 200 万次（与 bench/dict.hn / py/dict_lookup.py 等价）
local d = { a = 1, b = 2, c = 3 }
local i = 0
local s = 0
while i < 2000000 do
    if d["a"] ~= nil then
        s = s + 1
    end
    i = i + 1
end
print(s)
