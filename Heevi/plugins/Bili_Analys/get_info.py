import requests

headers = {
    'Acept-Language' : 'zh-CN,zh;q=0.7',
    'User-Agent' : 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36',
    }

def get_videoinfo(BV) :
    url = 'https://api.bilibili.com/x/web-interface/view'
    data = requests.get(url,params={
    'bvid' : BV
    },headers=headers,timeout = 10)
    video_data = data.json()
    return video_data

def b23tv_get(url:str):
    response = requests.get(url,headers=headers,timeout = 10)
    final_url = response.url
    Cut = final_url.index('BV')
    BV = final_url[Cut+2:Cut+12]
    return BV