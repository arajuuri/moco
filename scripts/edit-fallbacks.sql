-- How often the model edits files through the shell instead of the file tools (issue archive#22).
--
--   sqlite3 "$HOME/Library/Application Support/moco/data/moco.db" < scripts/edit-fallbacks.sql
--
-- One row per day: calls of the edit, write and patch tools, and `shell_file_writes`, the shell calls that show evidence
-- of changing a file: sed -i, perl -i, a python script that opens a file for writing or calls write_text/write_bytes,
-- `cat >`, `tee`, or echo/printf redirected into a file. Running a program is not counted: `python3 script.py`,
-- `python3 - <<EOF` that only prints or drives a terminal (`os.write(fd, ...)`), `cargo test`, `> /dev/null`.
--
-- It is a text match on the command: it can miss a rewrite, and `tee` also catches commands that only log output.
-- Compare the days before and after the edit-tool changes: `shell_file_writes` should fall against the tool calls.
-- scripts/edit-fallback-kinds.sql breaks the shell column down by kind and shows an example of each.
with calls as (
  select m.time_created as t,
         json_extract(p.value, '$.name') as name,
         json_extract(p.value, '$.input.command') as cmd
  from message m, json_each(m.data, '$.parts') p
  where m.type = 'assistant'
)
select date(t / 1000, 'unixepoch') as day,
       sum(name = 'edit')  as edit_calls,
       sum(name = 'write') as write_calls,
       sum(name = 'patch') as patch_calls,
       sum(name = 'shell' and (
             cmd like '%sed -i%' or cmd like '%perl -i%' or cmd like '%perl -pi%'
          or (cmd like '%python%' and (cmd like '%write_text(%' or cmd like '%write_bytes(%'
               or cmd like '%open(%''w''%' or cmd like '%open(%"w"%' or cmd like '%open(%''a''%' or cmd like '%open(%"a"%'
               or (cmd like '%open(%' and cmd like '%.write(%' and cmd not like '%os.write(%')))
          or cmd like '%cat >%' or cmd like '% tee %'
          or ((cmd like '%echo %' or cmd like '%printf %') and cmd like '% > %' and cmd not like '% > /dev/null%'))) as shell_file_writes
from calls
group by day order by day;
