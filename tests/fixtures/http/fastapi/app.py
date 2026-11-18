from fastapi import APIRouter, FastAPI

app = FastAPI()
router = APIRouter(prefix="/users")


@router.get("/{user_id}")
async def get_user(user_id: int):
    return {}


@router.post("")
def create_user():
    return {}


@app.api_route("/ping", methods=["GET", "HEAD"])
def ping():
    return "pong"


@app.get("/items/{item_id:path}")
def read_item(item_id: str):
    return {}


app.include_router(router, prefix="/api")
