-- HTTP 字节流与图像处理组合，不在 Image 插件内绑定下载或路由策略。
local Image=require('cosmos.image')
local http=require('cosmos.network.http')
local source=assert(Image.new(120,80,{r=30,g=90,b=150}))
local png=assert(source:encode('png'))
local server=assert(http.server.listen('127.0.0.1',0,function(req)
    if req:path()=='/source' then return {headers={['content-type']='image/png'},body=png} end
    if req:path()=='/resize' then
        local input=assert(Image.decode(assert(req:body()),{max_pixels=1000000}))
        local output=assert(input:thumbnail(60,60))
        return {headers={['content-type']='image/jpeg'},body=assert(output:encode('jpeg',{quality=95}))}
    end
    return {status=404}
end,{stream_body=true,max_body=1024*1024}))
local client=http.client.new()
local base='http://127.0.0.1:'..server:port()
local downloaded=assert(client:request('GET',base..'/source',{stream=true,max_body=1024*1024,timeout=2000}))
local image=assert(Image.decode(assert(downloaded.body:read_all(1024*1024))))
local patch=assert(image:crop(40,0,80,80))
local upload=assert(patch:encode('png'))
local at=1
local body=assert(http.body.from_callback(function(max)
    if at>#upload then return nil end
    local chunk=upload:sub(at,at+max-1);at=at+#chunk;return chunk
end))
local response=assert(client:request('POST',base..'/resize',{body=body,stream=true,max_body=1024*1024,timeout=2000}))
local result=assert(Image.decode(assert(response.body:read_all())))
assert(result:width()==60 and result:height()==60 and response.headers['content-type']=='image/jpeg')
server:close()
print('HTTP image download, stream upload, crop and thumbnail pipeline passed')
