(module $gcdemo
  (type $point (struct (field $x (mut i32)) (field $y f64) (field $next (ref null $point))))
  (type $arr (array (mut i32)))
  (global $g (mut (ref null $point)) (ref.null $point))
  (table $t 3 anyref)
  (memory 1)
  (data (i32.const 16) "hello, memory!")
  (func $use (param $p (ref null $point)) (param $a (ref null $arr)) (param $i i31ref) (result i32)
    (local $tmp anyref)
    (local.set $tmp (array.new $arr (i32.const 65) (i32.const 3)))
    (global.set $g (local.get $p))
    (table.set $t (i32.const 1) (local.get $a))
    local.get $p
    struct.get $point $x
    local.get $a
    i32.const 2
    array.get $arr
    i32.add)
  (func (export "_start")
    (call $use
      (struct.new $point (i32.const 1) (f64.const 2.5)
        (struct.new $point (i32.const 3) (f64.const 4.5) (ref.null $point)))
      (array.new $arr (i32.const 7) (i32.const 4))
      (ref.i31 (i32.const -42)))
    drop)
)
