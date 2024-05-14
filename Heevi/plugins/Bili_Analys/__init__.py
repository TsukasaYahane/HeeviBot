from nonebot import on_message
from nonebot.rule import regex
from nonebot.exception import FinishedException
from nonebot.adapters.onebot.v11 import MessageSegment,Event
from .get_info import get_videoinfo,b23tv_get
import re
import json

pattern = r'(b23.tv/|bilibili.com/video/|^BV[a-zA-Z0-9]{10})'

Bili_Analys = on_message(rule = regex(pattern,flags=re.I),priority=10,block=True)

@Bili_Analys.handle()    
async def Video_Analys(event:Event): 
    try :    
        content= str(event.get_message())
        try :
            data = json.loads(content)
            if data.get('title')=='哔哩哔哩' or '[QQ小程序]' in data.get('prompt') :
                qqdocurl = data['meta']['detail_1']['qqdocurl']
                Cut = qqdocurl.index('BV')
                BV = qqdocurl[Cut+2:Cut+12]
        except json.JSONDecodeError :
            url_pattern = re.compile(r'https?://(?:b23\.tv/\S+|)', re.IGNORECASE)
            urls = url_pattern.findall(content)
            if urls :
                url = urls[0]
                try :
                    if 'b23.tv' in url :
                        BV = b23tv_get(url)
                except Exception as e :
                    await Bili_Analys.finish(f"视频解析错误：{e}")
            else :
                Cut = content.index('BV')
                BV = content[Cut+2:Cut+12]
        data = get_videoinfo(BV)
        info = data['data']
        Video_Name = info['title']
        Video_Area = info['tname']
        Video_desc = info['desc']
        Video_Oner = info['owner']['name']
        video_data = info['stat']
        Video_view = video_data['view']
        Video_Like = video_data['like']
        Video_Danmu = video_data['danmaku']
        Video_Reply = video_data['reply']
        Video_Favorite = video_data['favorite']
        Video_Coin =video_data['coin']
        Video_Share = video_data['share']
        Video_pic = info['pic']
        await Bili_Analys.finish(MessageSegment.image(Video_pic)
                                 +MessageSegment.text("【视频标题】：{}\n"+"【视频分区||up主】：{}||{}\n"
                                 +"【播放量】：{}\t【点赞】：{}\t【投币】：{}\t【收藏】：{}\n"
                                 +"【评论】：{}\t【弹幕】：{}\t【分享】：{}\n【视频简介】：{}"
                                 .format(Video_Name,Video_Area,Video_Oner,Video_view,Video_Like,
                                  Video_Coin,Video_Favorite,Video_Reply,Video_Danmu,Video_Share,Video_desc)))
    except FinishedException :
        raise
    except Exception as e :
        await Bili_Analys.finish("视频解析错误：{}".format(e))


           
