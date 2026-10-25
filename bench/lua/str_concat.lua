-- 基准 4：字符串拼接 20 万次（与 bench/str.hn / py/str_concat.py 等价）
-- 语义等价：每轮 s = s .. "abc"（与 Python 的 s = s + "abc" 同为整串重建）
local s = ""
local i = 0
while i < 200000 do
    s = s .. "abc"
    i = i + 1
end
print(#s)
