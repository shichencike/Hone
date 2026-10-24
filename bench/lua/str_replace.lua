-- 基准 8：字符串替换 10 万次（与 bench/str_replace.hn / py/str_replace.py 等价）
-- gsub 全部替换语义 = Python str.replace；返回新串
local base = "abcabcabcabcabcabcabcabcabcabc"
local s = base
local i = 0
while i < 100000 do
    s = s:gsub("abc", "bca")
    i = i + 1
end
print(#s)
