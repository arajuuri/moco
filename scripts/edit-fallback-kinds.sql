-- Which kinds of file-changing shell command the model uses instead of the file tools (issue archive#22). Same evidence as
-- scripts/edit-fallbacks.sql, one row per kind with how many calls, and one example (the first alphabetically, cut to
-- 120 characters).
--
--   sqlite3 "$HOME/Library/Application Support/moco/data/moco.db" < scripts/edit-fallback-kinds.sql
--
-- The kind is the first one that matches, in the order listed. "python edit script" is the pattern the edit tool exists
-- for: read a file, replace text, write it back. The match is on text, so `tee` in particular also catches commands that
-- only log output.
with calls as (
  select json_extract(p.value, '$.input.command') as cmd
  from message m, json_each(m.data, '$.parts') p
  where m.type = 'assistant' and json_extract(p.value, '$.name') = 'shell'
),
flags as (
  select cmd,
    (cmd like '%python%') as py,
    (cmd like '%.read()%' or cmd like '%read_text(%') as reads,
    (cmd like '%write_text(%' or cmd like '%write_bytes(%' or cmd like '%open(%''w''%' or cmd like '%open(%"w"%'
       or cmd like '%open(%''a''%' or cmd like '%open(%"a"%'
       or (cmd like '%open(%' and cmd like '%.write(%' and cmd not like '%os.write(%')) as py_writes,
    (cmd like '%.replace(%' or cmd like '%re.sub(%' or cmd like '%count(%') as replaces
  from calls
),
kinds as (
  select cmd,
    case
      when cmd like '%sed -i%' then '1 sed -i'
      when cmd like '%perl -i%' or cmd like '%perl -pi%' then '2 perl -i'
      when py and py_writes and reads and replaces then '3 python edit script'
      when py and py_writes then '4 python writes a file'
      when cmd like '%cat >%' then '5 cat >'
      when cmd like '% tee %' then '6 tee'
      when (cmd like '%echo %' or cmd like '%printf %') and cmd like '% > %' and cmd not like '% > /dev/null%' then '7 echo/printf >'
    end as kind
  from flags
)
select kind, count(*) as n, min(substr(replace(cmd, char(10), ' '), 1, 120)) as example
from kinds where kind is not null
group by kind order by kind;
