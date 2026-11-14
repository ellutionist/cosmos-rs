-- XML 逻辑节点与原文编辑；可传临时父目录，额外验证 FS 原子写回。
local parent = ...
local xml = require('cosmos.encoding.xml')
assert(xml == cosmos.encoding.xml and xml.document == require('cosmos.encoding.xml.document'))
assert(xml.node == require('cosmos.encoding.xml.node'))
package.loaded['cosmos.encoding.xml']=nil
assert(require('cosmos.encoding.xml') == xml)
local raw = '<?xml version="1.0" encoding="UTF-8"?>\r\n<!--keep-->\n<movie a="x&#x9;y" xmlns:p="urn:test"><title>Old &amp; title</title><plot>A<![CDATA[<B>]]>C&#xD;D</plot><p:item p:id="7" id="plain"/><actor><name>Keep</name></actor><?note value?></movie>\n'
local bom='\239\187\191'
local bom_doc=assert(xml.parse(bom..'<x>bytes</x>'))
assert(bom_doc:encode() == bom..'<x>bytes</x>')
local first,last=bom_doc:root():range()
assert(first == 4 and bom_doc:encode():sub(first,last) == '<x>bytes</x>')
local doc = assert(xml.parse(raw))
assert(doc:encode() == raw)
local root = doc:root()
assert(root:kind() == 'element' and root:name() == 'movie' and root:namespace() == nil and root:parent() == nil)
assert(root:attribute('a') == 'x\ty')
local attrs=assert(root:attributes()); assert(#attrs == 1 and attrs[1].name == 'a')
assert(#root:elements() == 4 and root:child('title'):text() == 'Old & title')
local plot = root:child('plot')
assert(plot:text() == 'A<B>C\rD')
local pieces=plot:children()
assert(#pieces == 3 and pieces[1]:kind() == 'text' and pieces[2]:kind() == 'cdata')
assert(pieces[2]:raw() == '<![CDATA[<B>]]>' and pieces[3]:text() == 'C\rD')
assert(pieces[1]:parent():name() == 'plot')
local begin, finish = pieces[2]:range()
assert(raw:sub(begin, finish) == pieces[2]:raw())
local item = assert(root:child('item','urn:test'))
assert(item:name() == 'p:item' and item:prefix() == 'p' and item:local_name() == 'item' and item:namespace() == 'urn:test')
assert(item:attribute('id') == 'plain' and item:attribute('id','urn:test') == '7')
assert(root:child('item') == nil)
local standalone=assert(item:encode())
assert(xml.parse(standalone):root():namespace() == 'urn:test')
assert(standalone:find('xmlns:p="urn:test"',1,true))
local itemattrs=item:attributes(); assert(itemattrs[1].name == 'p:id' and itemattrs[1].namespace == 'urn:test' and itemattrs[2].name == 'id')
assert(doc:children()[1]:kind() == 'comment' or doc:children()[1]:kind() == 'text')
print('XML text, CDATA, attributes, namespaces and source ranges passed')

-- 标准节点构造按有序属性/子节点序列组合，不经 JSON 或对象字段扁平化。
local node=assert(xml.element('p:entry',{{name='xmlns:p',value='urn:new'},{name='quoted',value='" & <\t\n\r'}}, {
    'before & ', assert(xml.cdata('<raw>')), assert(xml.comment('kept')), assert(xml.pi('work','step=1')),
    assert(xml.element('b',nil,{'bold'})), assert(xml.text(' after\r')),
}))
assert(node:range() == nil and node:child('b'):children()[1]:parent():name() == 'b')
local built=assert(xml.document.new(node))
assert(built:root():attribute('quoted') == '" & <\t\n\r')
assert(built:root():text() == 'before & <raw>bold after\r')
assert(xml.parse(built:encode()):root():text() == built:root():text())
assert(xml.text(''):text() == '' and xml.text(''):encode() == '')
assert(xml.cdata(''):kind() == 'cdata' and xml.comment(''):kind() == 'comment')
assert(xml.document.new(xml.text('not a root')) == nil)

-- 文本替换保留其余原文；旧版本句柄仍可读，但不能编辑新版本。
local title=assert(xml.element('title',nil,{'Translated & <title>'}))
local updated=assert(doc:replace(root:child('title'),title))
local expected=raw:gsub('<title>Old &amp; title</title>','<title>Translated &amp; &lt;title&gt;</title>',1)
assert(updated:encode() == expected)
assert(doc:encode() == raw and root:child('title'):text() == 'Old & title')
assert(updated:replace(root:child('title'),title) == nil)
updated=assert(updated:replace(updated:root():child('plot'),nil))
assert(updated:root():child('plot') == nil)
local inserted=assert(xml.parse('<movie />'))
inserted=assert(inserted:insert(inserted:root(),title))
assert(inserted:root():child('title'):text() == 'Translated & <title>')
inserted=assert(inserted:insert(inserted:root(),xml.element('first'),1))
assert(inserted:root():elements()[1]:name() == 'first')
assert(inserted:insert(inserted:root(),title,1.5) == nil)
assert(inserted:replace(inserted:root(),nil) == nil)
local text_changed=assert(doc:replace(pieces[2],xml.text('x & y')))
assert(text_changed:root():child('plot'):text() == 'Ax & yC\rD')
assert(text_changed:encode():find('<plot>Ax &amp; yC&#xD;D</plot>',1,true))
local repeated=assert(xml.parse('<movie><title>one</title><!--keep--><title>two</title><actor><name>Unchanged</name></actor></movie>'))
for i=1,#repeated:root():elements('title') do
    repeated=assert(repeated:replace(repeated:root():elements('title')[i],title))
end
repeated=assert(repeated:insert(repeated:root(),xml.element('plot',nil,{'new plot'})))
assert(#repeated:root():elements('title') == 2 and repeated:root():child('plot'):text() == 'new plot')
assert(repeated:encode():find('<!--keep-->',1,true) and repeated:encode():find('<actor><name>Unchanged</name></actor>',1,true))
print('XML construction and precise immutable edits passed')

-- 跨命名空间移动须保留原语义，包括无命名空间元素进入默认命名空间。
local namespaced=assert(xml.parse('<outer xmlns="urn:default" xmlns:p="urn:other"><p:slot/></outer>'))
local moved=assert(namespaced:insert(namespaced:root(),item))
assert(moved:root():child('item','urn:test'):attribute('id','urn:test') == '7')
moved=assert(moved:insert(moved:root(),xml.element('plain',nil,{'value'})))
assert(moved:root():child('plain',''):namespace() == nil)
local qualified=assert(xml.parse('<a xmlns="urn:a"><b><c/></b></a>')):root():child('b','urn:a')
local copy=assert(xml.parse(qualified:encode()))
assert(copy:root():namespace() == 'urn:a' and copy:root():child('c','urn:a'))
print('Cross-document namespace preservation passed')

for _, bad in ipairs({
    '<!DOCTYPE movie><movie/>','<!DOCTYPE x SYSTEM "file:///etc/passwd"><x/>',
    '<!DOCTYPE x [<!ENTITY a "expanded">]><x>&a;</x>','<x>&unknown;</x>',
    '<?work?invalid?><a/>','<a/><b/>','<a><b></a>','<p:a/>','<x a="1" a="2"/>',
    '<x xmlns:p="urn:a" xmlns:q="urn:a" p:a="1" q:a="2"/>',
    '<x>\0</x>','<x>&#0;</x>','<x>\255</x>',
    '<?xml version="1.1"?><x/>','<?xml version="1.0" encoding="ISO-8859-1"?><x/>',
    '<x xmlns:xmlns="urn:wrong"/>','<x xmlns:xml="urn:wrong"/>','<x xmlns:p=""/>',
}) do assert(xml.parse(bad) == nil,bad:gsub('%z','NUL')) end
assert(xml.element('a x="injected"') == nil)
assert(xml.element('x',{{name='a="x" b',value='bad'}}) == nil)
assert(xml.element('x',{{name='a',value='1'},{name='a',value='2'}}) == nil)
assert(xml.element('p:x') == nil)
assert(xml.element('x',nil,{[2]='hole'}) == nil)
assert(xml.element('x',nil,{bad='mixed'}) == nil)
assert(xml.comment('bad--comment') == nil and xml.comment('trailing-') == nil)
assert(xml.cdata('bad]]>data') == nil and xml.pi('xml','reserved') == nil and xml.pi('XML','reserved') == nil)
assert(xml.pi('work','bad?>data') == nil and xml.pi('a:b','value') == nil)
assert(xml.parse('<!--<!DOCTYPE x>--><x/>'))
assert(xml.text('bad\0text') == nil)
local cyclic={}; cyclic[1]=cyclic
assert(xml.element('x',nil,cyclic) == nil)
assert(xml.element('x',nil,{['1']='string key'}) == nil)
local descendants=root:descendants(); assert(#descendants > #root:children() and descendants[1]:name() == 'title')
-- 大命名空间 URI 在节点间共享，转换属性列表时另检查展开后的输出预算。
local uri='urn:'..string.rep('u',262144)
local manyattrs={}
for i=1,65 do manyattrs[i]='p:a'..i..'="value"' end
local large_ns=assert(xml.parse('<r xmlns:p="'..uri..'" '..table.concat(manyattrs,' ')..'/>'))
assert(large_ns:root():attribute('a1',uri) == 'value')
assert(large_ns:root():attributes() == nil)
local huge=string.rep('a',1024*1024)
local repeats={}; for i=1,17 do repeats[i]=huge end
assert(xml.element('x',nil,repeats) == nil)
local deepest=string.rep('<x>',64)..string.rep('</x>',64)
assert(xml.parse(deepest))
assert(xml.parse('<x>'..deepest..'</x>') == nil)
assert(xml.parse(string.rep('<x>',10000)..string.rep('</x>',10000)) == nil)
local too_many='<x>'..string.rep('<c/>',100000)..'</x>'
assert(xml.parse(too_many) == nil)
assert(xml.parse(string.rep(' ',16*1024*1024+1)) == nil)
assert(xml.parse('<x>still usable</x>'):root():text() == 'still usable')
local unicode='\228\184\173\230\150\135'
local u=assert(xml.element(unicode,nil,{unicode}))
assert(u:name() == unicode and u:text() == unicode)
print('XML validation, resource limits and recovery passed')

if parent then
    local name=assert(cosmos.fs.path.join(parent,'cosmos-xml-' .. string.format('%.0f',cosmos.time.now_us()) .. '.xml'))
    assert(cosmos.fs.write_atomic(name,updated:encode(),{no_replace=true}))
    local reread=assert(xml.parse(assert(cosmos.fs.read_file(name))))
    assert(reread:root():child('title'):text() == title:text())
    assert(cosmos.fs.remove_file(name))
    print('XML atomic filesystem round-trip passed')
end
